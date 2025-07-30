use duckdb::Connection;
use log::info;

pub fn run_migrations(conn: &mut Connection) -> Result<(), duckdb::Error> {
    create_version_table(conn)?;
    let mut stmt = conn.prepare("SELECT version FROM db_version")?;
    let mut rows = stmt.query([])?;

    let current_version = if let Some(row) = rows.next()? {
        row.get(0)?
    } else {
        0
    };

    match current_version {
        0 => {
            create_initial_schema(conn)?;
        }
        /*1 => {
        migrate_to_version_2(conn)?;
        }*/
        _ => info!("database is up-to-date."),
    }

    Ok(())
}

pub fn create_version_table(conn: &mut Connection) -> Result<(), duckdb::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS db_version ( version INTEGER PRIMARY KEY);",
        [],
    )?;
    Ok(())
}

pub fn create_initial_schema(conn: &mut Connection) -> Result<(), duckdb::Error> {
    let initial_schema = r#"
    -- Table of information about the oracle, mostly to prevent multiple keys from being used with the same database
    -- singleton_constant is a dummy column to ensure there is only one row
    CREATE TABLE IF NOT EXISTS oracle_metadata
    (
            pubkey             BLOB     NOT NULL UNIQUE PRIMARY KEY,
            name               TEXT      NOT NULL UNIQUE,
            created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            singleton_constant BOOLEAN   NOT NULL DEFAULT TRUE, -- make sure there is only one row
            CONSTRAINT one_row_check UNIQUE (singleton_constant)
    );

    CREATE TABLE IF NOT EXISTS events (
          id UUID PRIMARY KEY,
          total_allowed_entries INTEGER NOT NULL,
          number_of_places_win INTEGER NOT NULL,
          number_of_values_per_entry INTEGER NOT NULL,
          signing_date TIMESTAMPTZ NOT NULL,
          start_observation_date  TIMESTAMPTZ NOT NULL,
          end_observation_date  TIMESTAMPTZ NOT NULL,
          created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
          updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
          nonce BLOB NOT NULL,
          event_announcement BLOB NOT NULL,
          locations TEXT[] NOT NULL,
          coordinator_pubkey TEXT,
          attestation_signature BLOB
    );

    CREATE TYPE options AS ENUM ('over', 'par', 'under');

    CREATE TABLE IF NOT EXISTS events_entries
    (
        id UUID PRIMARY KEY,
        event_id UUID NOT NULL REFERENCES events (id),
        score INTEGER NOT NULL DEFAULT 0,
        base_score INTEGER NOT NULL DEFAULT 0,
        created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    );

    CREATE SEQUENCE id_sequence START 1000;
    CREATE TABLE IF NOT EXISTS expected_observations
    (
        id BIGINT DEFAULT nextval('id_sequence') PRIMARY KEY,
        entry_id UUID NOT NULL REFERENCES events_entries(id),
        station TEXT NOT NULL,
        temp_low options,
        temp_high options,
        wind_speed options
    );

    CREATE TABLE IF NOT EXISTS weather
    (
        id UUID PRIMARY KEY,
        station_id TEXT NOT NULL,
        observed STRUCT(reading_date TIMESTAMPTZ, temp_low INTEGER, temp_high INTEGER, wind_speed INTEGER),
        forecasted STRUCT(reading_date TIMESTAMPTZ, temp_low INTEGER, temp_high INTEGER, wind_speed INTEGER),
        created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    );

    CREATE TABLE IF NOT EXISTS events_weather
    (
        id UUID PRIMARY KEY,
        event_id UUID NOT NULL REFERENCES events (id),
        weather_id UUID NOT NULL REFERENCES weather (id),
        created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    );

    INSERT INTO db_version (version) VALUES (1);
    "#;
    conn.execute_batch(initial_schema)?;
    Ok(())
}

/* how to add the next sql migration:
pub fn migrate_to_version_2(conn: &mut Connection) -> Result<(), duckdb::Error> {
    let migration_2 = r#"
    UPDATE db_version SET version = 2;"#;"
    conn.execute_batch(migration_2)?;
    Ok(())
}
*/
