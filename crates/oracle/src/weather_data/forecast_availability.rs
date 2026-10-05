//! Fast discovery screening over native metric rows. Event creation still
//! verifies retained source documents through the settlement assessment.
use super::*;

fn availability_sql(
    rows: &str,
    start: &str,
    end: &str,
    metrics: &[String],
) -> Result<String, Error> {
    let mut checks = Vec::new();
    for metric in metrics {
        let field = match metric.as_str() {
            "temp_high" => "max_temp",
            "temp_low" => "min_temp",
            "wind_speed" => "wind_speed",
            "wind_direction" => "wind_direction",
            "humidity" => "relative_humidity_max",
            "rain_amt" => "liquid_precipitation_amt",
            "snow_amt" => "snow_amt",
            _ => {
                return Err(Error::ForecastQuality {
                    reason: "unknown forecast metric".into(),
                });
            }
        };
        // Missing native values retain their layout. Aggregation must not
        // turn one missing sample into a usable maximum of the other samples.
        let named = format!(
            "({field} IS NOT NULL OR json_exists(TRY_CAST(source_layouts AS JSON), '$.{field}'))"
        );
        let inside = format!(
            "{named} AND middle >= '{start}'::TIMESTAMPTZ AND middle < '{end}'::TIMESTAMPTZ"
        );
        checks.push(format!("COUNT(*) FILTER (WHERE {inside}) > 0 AND COUNT(*) FILTER (WHERE {inside} AND ({field} IS NULL OR NOT isfinite({field}) OR quality_status = 'rejected')) = 0"));
        if metric == "wind_speed" || metric == "wind_direction" {
            checks.push(format!("MIN(begin_ts) FILTER (WHERE wind_speed IS NOT NULL) <= '{start}'::TIMESTAMPTZ + INTERVAL 1 HOUR AND MAX(begin_ts) FILTER (WHERE wind_speed IS NOT NULL) >= '{end}'::TIMESTAMPTZ"));
        }
    }
    Ok(format!(
        r#"
        WITH latest AS (
            SELECT *, begin_ts + (end_ts - begin_ts) / 2 AS middle FROM ({rows})
            QUALIFY DENSE_RANK() OVER (PARTITION BY station_id ORDER BY generated_ts DESC NULLS LAST, published_ts DESC NULLS LAST) = 1
        )
        SELECT station_id::VARCHAR AS station_id FROM latest GROUP BY station_id
        HAVING {}
    "#,
        checks.join(" AND ")
    ))
}

impl WeatherAccess {
    pub(super) async fn available_forecasts(
        &self,
        request: &ForecastRequest,
        station_ids: Vec<String>,
        metrics: &[String],
    ) -> Result<Vec<String>, Error> {
        if station_ids.is_empty() {
            return Ok(Vec::new());
        }
        let (Some(start), Some(end)) = (request.start, request.end) else {
            return Err(Error::ForecastQuality {
                reason: "forecast screening needs a window".into(),
            });
        };
        let now = OffsetDateTime::now_utc();
        let cutoff = start.min(now);
        let issued = (cutoff - Duration::hours(3), cutoff);
        let names = self
            .file_access
            .grab_file_names(forecast_file_params(issued, now))
            .await?;
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let conditions = format!(
            "generated_ts >= '{}'::TIMESTAMPTZ AND generated_ts <= '{}'::TIMESTAMPTZ",
            issued.0.format(&Rfc3339)?,
            cutoff.format(&Rfc3339)?
        );
        let stations = station_condition(&station_ids)?;
        let rows = self.forecast_rows_sql(names, issued, stations.as_deref(), &conditions);
        let sql = availability_sql(
            &rows,
            &start.format(&Rfc3339)?,
            &end.format(&Rfc3339)?,
            metrics,
        )?;
        self.query_with_connection(sql, |_, batches| {
            let mut ids = Vec::new();
            for batch in batches {
                let column = strings(batch, "station_id")?;
                ids.extend(column.iter().flatten().map(str::to_owned));
            }
            Ok(ids)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_missing_native_wind_value_excludes_the_station() {
        let rows = r#"SELECT station_id, ts AS begin_ts, ts AS end_ts,
            TIMESTAMPTZ '2030-01-01 00:00:00+00' AS generated_ts,
            TIMESTAMPTZ '2030-01-01 00:00:00+00' AS published_ts,
            CASE WHEN station_id = 'KGAP' AND hour(ts) = 3 THEN NULL ELSE 10 END AS wind_speed,
            '{"wind_speed":{}}' AS source_layouts, 'validated' AS quality_status
            FROM (VALUES ('KGOOD'), ('KGAP')) AS stations(station_id),
            generate_series(TIMESTAMPTZ '2030-01-01 00:00:00+00', TIMESTAMPTZ '2030-01-02 00:00:00+00', INTERVAL 1 HOUR) AS periods(ts)"#;
        let sql = availability_sql(
            rows,
            "2030-01-01T00:00:00Z",
            "2030-01-02T00:00:00Z",
            &["wind_speed".into()],
        )
        .unwrap();
        let connection = open_connection().unwrap();
        let ids: Vec<String> = connection
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ids, ["KGOOD"]);
    }
}
