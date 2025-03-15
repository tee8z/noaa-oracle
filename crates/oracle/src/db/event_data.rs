use super::{run_migrations, CreateEventData, Event, EventFilter, EventSummary};

use crate::{
    ActiveEvent, Forecasted, Observed, SignEvent, ToRawSql, ValueOptions, Weather, WeatherChoices,
    WeatherChoicesWithEntry, WeatherEntry,
};
use dlctix::musig2::secp256k1::XOnlyPublicKey;
use duckdb::types::Value;
use duckdb::{params, params_from_iter, AccessMode, Config, Connection};
use log::{debug, info};
use regex::Regex;
use scooby::postgres::{insert_into, select, update, with, Aliasable, Joinable, Parameters};
use serde_json::to_vec;
use std::collections::HashMap;
use std::time::Duration as StdDuration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

pub struct EventData {
    connection_path: String,
    retry_duration: StdDuration,
    retry_max_attemps: i32,
}

impl EventData {
    pub fn new(path: &str) -> Result<Self, duckdb::Error> {
        let connection_path = format!("{}/events.db3", path);
        let mut conn = Connection::open(connection_path.clone())?;
        run_migrations(&mut conn)?;
        Ok(Self {
            connection_path,
            retry_duration: StdDuration::from_millis(100),
            retry_max_attemps: 5,
        })
    }

    async fn new_readonly_connection(&self) -> Result<Connection, duckdb::Error> {
        let config = Config::default().access_mode(AccessMode::ReadOnly)?;
        Connection::open_with_flags(self.connection_path.clone(), config)
    }

    pub async fn new_readonly_connection_retry(&self) -> Result<Connection, duckdb::Error> {
        let mut attempt = 0;
        loop {
            match timeout(self.retry_duration, self.new_readonly_connection()).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => {
                    if attempt >= self.retry_max_attemps
                        || !e.to_string().contains("Could not set lock on file")
                    {
                        return Err(e);
                    }
                    info!("Retrying: {}", e);
                    attempt += 1;
                }
                Err(_) => {
                    return Err(duckdb::Error::DuckDBFailure(
                        duckdb::ffi::Error {
                            code: duckdb::ErrorCode::DatabaseLocked,
                            extended_code: 0,
                        },
                        None,
                    ));
                }
            }
        }
    }

    async fn new_write_connection(&self) -> Result<Connection, duckdb::Error> {
        let config = Config::default().access_mode(AccessMode::ReadWrite)?;
        Connection::open_with_flags(self.connection_path.clone(), config)
    }

    pub async fn new_write_connection_retry(&self) -> Result<Connection, duckdb::Error> {
        let mut attempt = 0;
        loop {
            match timeout(self.retry_duration, self.new_write_connection()).await {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(e)) => {
                    if attempt >= self.retry_max_attemps
                        || !e.to_string().contains("Could not set lock on file")
                    {
                        return Err(e);
                    }
                    info!("Retrying: {}", e);
                    attempt += 1;
                }
                Err(_) => {
                    return Err(duckdb::Error::DuckDBFailure(
                        duckdb::ffi::Error {
                            code: duckdb::ErrorCode::DatabaseLocked,
                            extended_code: 0,
                        },
                        None,
                    ));
                }
            }
        }
    }

    pub async fn get_stored_public_key(&self) -> Result<XOnlyPublicKey, duckdb::Error> {
        let select = select("pubkey").from("oracle_metadata");
        let conn = self.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&select.to_string())?;
        let key: Vec<u8> = stmt.query_row([], |row| row.get(0))?;
        //TODO: use a custom error here so we don't need to panic
        let converted_key = XOnlyPublicKey::from_slice(&key).expect("invalid pubkey");
        Ok(converted_key)
    }

    pub async fn add_oracle_metadata(&self, pubkey: XOnlyPublicKey) -> Result<(), duckdb::Error> {
        let pubkey_raw = pubkey.serialize().to_vec();
        //TODO: Add the ability to change the name via config
        let name = String::from("4casttruth");
        let conn = self.new_write_connection_retry().await?;
        let mut stmt = conn.prepare("INSERT INTO oracle_metadata (pubkey,name) VALUES(?,?)")?;
        stmt.execute([pubkey_raw, name.into()])?;
        Ok(())
    }

    // Call as an ETL process to update the weather for running events
    pub async fn update_weather_station_data(
        &self,
        event_id: Uuid,
        weather: Vec<Weather>,
    ) -> Result<(), duckdb::Error> {
        //1) grab events that are using this weather data
        //2) add new weather data to table
        let weather_ids = self.add_weather_readings(weather).await?;

        //3) create join between weather and events
        self.batch_add_weather_to_event(event_id, weather_ids)
            .await?;

        Ok(())
    }

    pub async fn add_weather_readings(
        &self,
        weather: Vec<Weather>,
    ) -> Result<Vec<Uuid>, duckdb::Error> {
        let params: Vec<(Uuid, Value, Forecasted, Option<Observed>)> = weather
            .iter()
            .map(|weather| {
                let weather_id = Uuid::now_v7();
                (
                    weather_id,
                    Value::Text(weather.station_id.clone()),
                    weather.forecasted.clone(),
                    weather.observed.clone(),
                )
            })
            .collect();
        let weather_ids: Vec<Uuid> = params.iter().map(|row| row.0).collect();
        let mut param_placeholders = Parameters::new();
        let params_values: Vec<(String, String, String, String)> = params
            .iter()
            .map(|vals| {
                (
                    param_placeholders.next(),
                    param_placeholders.next(),
                    vals.2.to_raw_sql(),
                    vals.3
                        .clone()
                        .map_or("Null".to_string(), |x| x.to_raw_sql()),
                )
            })
            .collect();

        let insert_weather = insert_into("weather")
            .columns(("id", "station_id", "forecasted", "observed"))
            .values(params_values);
        let query_str = self.prepare_query(insert_weather.to_string());
        debug!("query_str: {}", query_str);
        let insert_values: Vec<Value> = params
            .into_iter()
            .flat_map(|(a, b, _, _)| vec![Value::Text(a.to_string()), b])
            .collect();
        debug!("insert values: {:?}", insert_values);

        let conn = self.new_write_connection_retry().await?;
        let mut weather_stmt = conn.prepare(&query_str)?;
        weather_stmt.execute(params_from_iter(insert_values.iter()))?;
        Ok(weather_ids)
    }

    pub async fn batch_add_weather_to_event(
        &self,
        event_id: Uuid,
        weather_ids: Vec<Uuid>,
    ) -> Result<(), duckdb::Error> {
        let params: Vec<(String, Uuid, String)> = weather_ids
            .iter()
            .map(|weather_id| {
                let event_weather_id = Uuid::now_v7().to_string();
                (event_weather_id, event_id, weather_id.to_string())
            })
            .collect();
        let mut param_placeholders = Parameters::new();
        let params_values: Vec<(String, String, String)> = params
            .iter()
            .map(|_| {
                (
                    param_placeholders.next(),
                    param_placeholders.next(),
                    param_placeholders.next(),
                )
            })
            .collect();

        let insert_event_weather = insert_into("events_weather")
            .columns(("id", "event_id", "weather_id"))
            .values(params_values);
        let query_str = self.prepare_query(insert_event_weather.to_string());
        debug!("query_str: {}", query_str);
        let insert_values: Vec<String> = params
            .into_iter()
            .flat_map(|(a, b, c)| vec![a, b.to_string(), c])
            .collect();

        debug!("insert values: {:?}", insert_values);

        let conn = self.new_write_connection_retry().await?;
        let mut weather_stmt = conn.prepare(&query_str)?;
        weather_stmt.execute(params_from_iter(insert_values.iter()))?;
        Ok(())
    }
    pub async fn add_event(&self, event: CreateEventData) -> Result<Event, duckdb::Error> {
        let locations_sql = format!("[{}]", event.locations.join(","));

        let signing_date = OffsetDateTime::format(event.signing_date, &Rfc3339)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
        let observation_date = OffsetDateTime::format(event.observation_date, &Rfc3339)
            .map_err(|e| duckdb::Error::ToSqlConversionFailure(Box::new(e)))?;
        let nonce = to_vec(&event.nonce).unwrap();
        let announcement_bytes = to_vec(&event.event_announcement).unwrap();
        let conn = self.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(
            "INSERT INTO events (
                id,
                total_allowed_entries,
                number_of_places_win,
                number_of_values_per_entry,
                nonce,
                signing_date,
                observation_date,
                locations,
                event_announcement,
                coordinator_pubkey) VALUES(?,?,?,?,?,?,?,?,?,?)",
        )?;
        stmt.execute(params![
            event.id.to_string(),
            event.total_allowed_entries,
            event.number_of_places_win,
            event.number_of_values_per_entry,
            nonce,
            signing_date,
            observation_date,
            locations_sql,
            announcement_bytes,
            event.coordinator_pubkey
        ])?;

        Ok(event.into())
    }

    pub async fn get_event_coordinator_pubkey(
        &self,
        event_id: Uuid,
    ) -> Result<String, duckdb::Error> {
        let coordinator_pubkey = select("coordinator_pubkey")
            .from("events")
            .where_("id = $1");
        let query_str = self.prepare_query(coordinator_pubkey.to_string());
        debug!("query_str: {}", query_str);
        let conn = self.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let sql_params = params_from_iter(vec![event_id.to_string()]);
        stmt.query_row(sql_params, |row| row.get(0))
    }

    pub async fn add_event_entry(
        &self,
        entry: WeatherEntry,
    ) -> Result<WeatherEntry, duckdb::Error> {
        //TODO: have these transactions happen in the same transaction
        self.add_entry(entry.clone()).await?;
        self.add_entry_choices(entry.clone()).await?;
        Ok(entry)
    }

    pub async fn add_entry(&self, entry: WeatherEntry) -> Result<(), duckdb::Error> {
        let conn = self.new_write_connection_retry().await?;
        // TODO: add check on the INSERT transaction to verify we never go over number of allowed entries in an event
        // (current worse case is just a couple of extra entries made it into the event, doesn't change how we sign the result)
        let insert_query = "INSERT INTO events_entries (id, event_id) VALUES(?,?)";
        let mut event_stmt = conn.prepare(insert_query)?;

        debug!("query_str: {}", insert_query);
        let insert_values = params![entry.id.to_string(), entry.event_id.to_string()];

        event_stmt.execute(insert_values)?;
        Ok(())
    }

    pub async fn add_entry_choices(&self, entry: WeatherEntry) -> Result<(), duckdb::Error> {
        #[allow(clippy::type_complexity)]
        let params: Vec<(
            Uuid,
            String,
            Option<ValueOptions>,
            Option<ValueOptions>,
            Option<ValueOptions>,
        )> = entry
            .expected_observations
            .iter()
            .map(|weather_choices| {
                (
                    entry.id,
                    weather_choices.stations.clone(),
                    weather_choices.temp_low.clone(),
                    weather_choices.temp_high.clone(),
                    weather_choices.wind_speed.clone(),
                )
            })
            .collect();

        let mut param_placeholders = Parameters::new();
        let params_values: Vec<(String, String, String, String, String)> = params
            .iter()
            .map(|_| {
                (
                    param_placeholders.next(),
                    param_placeholders.next(),
                    param_placeholders.next(),
                    param_placeholders.next(),
                    param_placeholders.next(),
                )
            })
            .collect();

        let insert_event_weather = insert_into("expected_observations")
            .columns(("entry_id", "station", "temp_low", "temp_high", "wind_speed"))
            .values(params_values);
        let query_str = self.prepare_query(insert_event_weather.to_string());
        debug!("query_str: {}", query_str);
        let insert_values: Vec<Value> = params
            .into_iter()
            .flat_map(|(a, b, c, d, e)| {
                let temp_low = match c {
                    Some(c) => Value::Text(c.to_string()),
                    _ => Value::Null,
                };
                let temp_high = match d {
                    Some(d) => Value::Text(d.to_string()),
                    _ => Value::Null,
                };
                let wind_speed = match e {
                    Some(e) => Value::Text(e.to_string()),
                    _ => Value::Null,
                };
                vec![
                    Value::Text(a.to_string()),
                    Value::Text(b),
                    temp_low,
                    temp_high,
                    wind_speed,
                ]
            })
            .collect();

        info!("insert values: {:?}", insert_values);
        if insert_values.is_empty() {
            debug!("entry values were emtpy, skipping creating entry");
            return Ok(());
        }

        let conn = self.new_write_connection_retry().await?;
        let mut weather_stmt = conn.prepare(&query_str)?;
        weather_stmt.execute(params_from_iter(insert_values.iter()))?;
        Ok(())
    }
    pub async fn update_event_attestation(&self, event: &SignEvent) -> Result<(), duckdb::Error> {
        let entry_score_update_query = update("events")
            .set("attestation_signature", "$1")
            .where_("events.id = $2");

        let query_str = self.prepare_query(entry_score_update_query.to_string());
        debug!("query_str: {}", query_str);

        let conn = self.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;

        let Some(attestation) = event.attestation else {
            return Err(duckdb::Error::InvalidParameterCount(1, 2));
        };
        let attestation_bytes = to_vec(&attestation).unwrap();
        stmt.execute(params![attestation_bytes, event.id.to_string()])?;
        Ok(())
    }

    ///Danger: a raw SQL query is used, input is not escaped with '?'
    pub async fn update_entry_scores(
        &self,
        entry_scores: Vec<(Uuid, i64)>,
    ) -> Result<(), duckdb::Error> {
        let number_entry_scores = entry_scores.len();
        info!("number_entry_scores: {:?}", number_entry_scores);

        let mut entry_score_values = String::new();
        entry_score_values.push_str("VALUES");
        for (index, val) in entry_scores.iter().enumerate() {
            entry_score_values.push_str(&format!("('{}',{})", val.0, val.1));
            if index + 1 < number_entry_scores {
                entry_score_values.push(',');
            }
        }

        info!("entry_score_values: {}", entry_score_values);

        let mut entry_ids = String::new();
        entry_ids.push('(');
        for (index, val) in entry_scores.iter().enumerate() {
            entry_ids.push_str(&format!("'{}'", &val.0.to_string()));
            if index + 1 < number_entry_scores {
                entry_ids.push(',');
            }
        }
        entry_ids.push(')');
        info!("entry_ids: {}", entry_ids);
        let scores_temp_select = select("score")
            .from((entry_score_values).as_("scores(entry_id, score)"))
            .where_("scores.entry_id = events_entries.id::TEXT")
            .to_string();
        let entry_score_update_query = update("events_entries")
            .set("score", format!("({})", scores_temp_select))
            .where_(format!("events_entries.id::TEXT IN {}", entry_ids));

        let query_str = entry_score_update_query.to_string();
        debug!("query_str: {}", query_str);

        let conn = self.new_write_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        stmt.execute([])?;
        Ok(())
    }

    pub async fn get_event_weather(&self, event_id: Uuid) -> Result<Vec<Weather>, duckdb::Error> {
        let event_weather = select(("station_id", "observed", "forecasted"))
            .from(
                "events_weather"
                    .join("events")
                    .on("events_weather.event_id = events.id")
                    .join("weather")
                    .on("weather.id = events_weather.weather_id"),
            )
            .where_("event_id = ?");
        let query_str = event_weather.to_string();
        debug!("query_str: {}", query_str);

        let conn = self.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut event_weather_rows = stmt.query([event_id.to_string()])?;
        let mut event_weather = vec![];
        while let Some(row) = event_weather_rows.next()? {
            let data: Weather = row.try_into()?;
            event_weather.push(data);
        }
        Ok(vec![])
    }

    pub async fn get_event_weather_entries(
        &self,
        event_id: &Uuid,
    ) -> Result<Vec<WeatherEntry>, duckdb::Error> {
        // Query 1
        let event_entries_select =
            select(("events_entries.id", "events_entries.event_id", "score"))
                .from(
                    "events_entries"
                        .join("events")
                        .on("events_entries.event_id = events.id"),
                )
                .where_("events_entries.event_id = ?")
                .group_by(("events_entries.id", "events_entries.event_id", "score"));

        let query_str = event_entries_select.to_string();
        debug!("query_str: {}", query_str);

        let conn = self.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let mut weather_entry_rows = stmt.query([event_id.to_string()])?;
        let mut weather_entries = vec![];
        while let Some(row) = weather_entry_rows.next()? {
            let data: WeatherEntry = row.try_into()?;
            weather_entries.push(data);
        }

        // Query 2
        let entry_choices = select((
            "entry_id",
            "station",
            "temp_low::TEXT",
            "temp_high::TEXT",
            "wind_speed::TEXT",
        ))
        .from(
            "expected_observations"
                .join("events_entries")
                .on("events_entries.id = expected_observations.entry_id"),
        )
        .where_("events_entries.event_id = $1");
        let entry_choices_query_str = self.prepare_query(entry_choices.to_string());
        debug!("query_str: {}", entry_choices_query_str);
        let mut stmt_choices = conn.prepare(&entry_choices_query_str)?;
        let mut rows = stmt_choices.query([event_id.to_string()])?;

        //Combine query results
        let mut weather_choices: HashMap<Uuid, Vec<WeatherChoices>> = HashMap::new();
        while let Some(row) = rows.next()? {
            let data: WeatherChoicesWithEntry = row.try_into()?;
            if let Some(entry_choices) = weather_choices.get_mut(&data.entry_id) {
                entry_choices.push(data.into());
            } else {
                weather_choices.insert(data.entry_id, vec![data.into()]);
            }
        }

        for weather_entry in weather_entries.iter_mut() {
            if let Some(choices) = weather_choices.get(&weather_entry.id) {
                weather_entry.expected_observations = choices.clone();
            }
        }

        Ok(weather_entries)
    }

    pub async fn get_weather_entry(
        &self,
        event_id: &Uuid,
        entry_id: &Uuid,
    ) -> Result<WeatherEntry, duckdb::Error> {
        // Query 1
        let event_entry = select((
            "events_entries.id as id",
            "events_entries.event_id as event_id",
            "score",
        ))
        .from("events_entries")
        .where_("events_entries.id = $1 AND events_entries.event_id = $2");

        let conn = self.new_readonly_connection_retry().await?;
        let query_str = self.prepare_query(event_entry.to_string());
        debug!("query_str: {}", query_str);

        let mut stmt = conn.prepare(&query_str)?;
        let sql_params_entry = params_from_iter(vec![entry_id.to_string(), event_id.to_string()]);
        let mut weather_entry: WeatherEntry =
            stmt.query_row(sql_params_entry, |row| row.try_into())?;

        // Query 2
        let entry_choices = select((
            "station",
            "temp_low::TEXT",
            "temp_high::TEXT",
            "wind_speed::TEXT",
        ))
        .from("expected_observations")
        .where_("expected_observations.entry_id = $1");
        let entry_choices_query_str = self.prepare_query(entry_choices.to_string());
        debug!("query_str: {}", entry_choices_query_str);
        let sql_params = params_from_iter(vec![entry_id.to_string()]);

        let mut stmt_choices = conn.prepare(&entry_choices_query_str)?;
        let mut rows = stmt_choices.query(sql_params)?;
        let mut weather_choices: Vec<WeatherChoices> = vec![];
        while let Some(row) = rows.next()? {
            let data: WeatherChoices = row.try_into()?;
            weather_choices.push(data);
        }

        weather_entry.expected_observations = weather_choices;
        Ok(weather_entry)
    }

    pub async fn filtered_list_events(
        &self,
        filter: EventFilter,
    ) -> Result<Vec<EventSummary>, duckdb::Error> {
        let mut events = self.get_filtered_event_summarys(filter).await?;
        for event in events.iter_mut() {
            event.weather = self.get_event_weather(event.id).await?;
        }
        Ok(events)
    }

    async fn get_filtered_event_summarys(
        &self,
        filter: EventFilter,
    ) -> Result<Vec<EventSummary>, duckdb::Error> {
        let event_entries_select = select(("Count(id) as total_entries", "event_id"))
            .from("events_entries")
            .group_by("event_id");

        let mut event_select = with("event_entries")
            .as_(event_entries_select)
            .select((
                "id",
                "signing_date::TEXT",
                "observation_date::TEXT",
                "locations",
                "total_allowed_entries",
                "COALESCE(event_entries.total_entries,0) as total_entries",
                "number_of_places_win",
                "number_of_values_per_entry",
                "attestation_signature",
                "nonce",
            ))
            .from(
                "events"
                    .left_join("event_entries")
                    .on("event_entries.event_id = events.id"),
            );
        if let Some(ids) = filter.event_ids.clone() {
            let mut event_ids_val = String::new();
            event_ids_val.push('(');
            for (index, _) in ids.iter().enumerate() {
                event_ids_val.push('?');
                if index < ids.len() {
                    event_ids_val.push(',');
                }
            }
            event_ids_val.push(')');
            let where_clause = format!("events.id IN {}", event_ids_val);
            event_select = event_select.clone().where_(where_clause);
        }
        if let Some(limit) = filter.limit {
            event_select = event_select.clone().limit(limit);
        }

        let conn = self.new_readonly_connection_retry().await?;
        let query_str = self.prepare_query(event_select.to_string());
        debug!("query_str: {}", query_str);
        let mut stmt = conn.prepare(&query_str)?;
        let mut rows = if let Some(ids) = filter.event_ids {
            let params: Vec<Value> = ids
                .iter()
                .map(|event_id| Value::Text(event_id.to_string()))
                .collect();
            stmt.query(params_from_iter(params.iter()))
        } else {
            stmt.query([])
        }?;
        let mut event_data: Vec<EventSummary> = vec![];
        while let Some(row) = rows.next()? {
            let data: EventSummary = row.try_into()?;
            event_data.push(data.clone());
        }

        Ok(event_data)
    }

    pub async fn get_event(&self, id: &Uuid) -> Result<Event, duckdb::Error> {
        let mut event = self.get_basic_event(id).await?;
        info!("event: {:?}", event);
        let weather_entries: Vec<WeatherEntry> = self.get_event_weather_entries(id).await?;
        event.entries = weather_entries.clone();
        event.entry_ids = weather_entries.iter().map(|val| val.id).collect();
        let event_weather: Vec<Weather> = self.get_event_weather(event.id).await?;
        event.weather = event_weather;
        info!("events: {:?}", event);
        Ok(event)
    }

    async fn get_basic_event(&self, id: &Uuid) -> Result<Event, duckdb::Error> {
        let event_select = select((
            "id",
            "signing_date::TEXT",
            "observation_date::TEXT",
            "event_announcement",
            "locations",
            "total_allowed_entries",
            "number_of_places_win",
            "number_of_values_per_entry",
        ))
        .and_select(("attestation_signature", "nonce", "coordinator_pubkey"))
        .from("events")
        .where_("id = $1");

        let query_str = self.prepare_query(event_select.to_string());
        debug!("query_str: {}", query_str);
        let conn = self.new_readonly_connection_retry().await?;
        let mut stmt = conn.prepare(&query_str)?;
        let sql_params = params_from_iter(vec![id.to_string()]);
        stmt.query_row(sql_params, |row| row.try_into())
    }

    pub async fn get_active_events(&self) -> Result<Vec<ActiveEvent>, duckdb::Error> {
        let event_entries_select = select(("Count(id) as total_entries", "event_id"))
            .from("events_entries")
            .group_by("event_id");

        let event_select = with("event_entries")
            .as_(event_entries_select)
            .select((
                "id",
                "signing_date::TEXT",
                "observation_date::TEXT",
                "locations",
                "total_allowed_entries",
                "COALESCE(event_entries.total_entries, 0) as total_entries",
                "number_of_places_win",
                "number_of_values_per_entry",
                "attestation_signature",
            ))
            .from(
                "events"
                    .left_join("event_entries")
                    .on("event_entries.event_id = events.id"),
            )
            .where_("attestation_signature IS NULL"); //Only filter out events that have been signed

        let conn = self.new_readonly_connection_retry().await?;
        let query_str = self.prepare_query(event_select.to_string());
        debug!("query_str: {}", query_str);
        let mut stmt = conn.prepare(&query_str)?;

        let mut rows = stmt.query([])?;
        let mut event_data: Vec<ActiveEvent> = vec![];
        while let Some(row) = rows.next()? {
            let data: ActiveEvent = row.try_into()?;
            event_data.push(data);
        }

        Ok(event_data)
    }

    pub async fn get_events_to_sign(
        &self,
        event_ids: Vec<Uuid>,
    ) -> Result<Vec<SignEvent>, duckdb::Error> {
        let mut event_ids_val = String::new();
        event_ids_val.push('(');
        for (index, _) in event_ids.iter().enumerate() {
            event_ids_val.push('?');
            if index + 1 < event_ids.len() {
                event_ids_val.push(',');
            }
        }
        event_ids_val.push(')');
        let where_clause = format!(
            "attestation_signature IS NULL AND events.id IN {}",
            event_ids_val
        );

        let event_select = select((
            "id",
            "signing_date::TEXT",
            "observation_date::TEXT",
            "number_of_places_win",
            "number_of_values_per_entry",
            "attestation_signature",
            "nonce",
            "event_announcement",
        ))
        .from("events")
        .where_(where_clause);

        let params: Vec<Value> = event_ids
            .iter()
            .map(|event_id| Value::Text(event_id.to_string()))
            .collect();

        let conn = self.new_readonly_connection_retry().await?;
        let query_str = self.prepare_query(event_select.to_string());
        debug!("query_str: {}", query_str);
        let mut stmt = conn.prepare(&query_str)?;

        let mut rows = stmt.query(params_from_iter(params.iter()))?;
        let mut event_data: Vec<SignEvent> = vec![];
        while let Some(row) = rows.next()? {
            let data: SignEvent = row.try_into()?;
            event_data.push(data);
        }

        Ok(event_data)
    }

    fn prepare_query(&self, query: String) -> String {
        let re = Regex::new(r"\$(\d+)").unwrap();
        let fixed_params = re.replace_all(&query, "?");
        fixed_params.to_string()
    }
}
