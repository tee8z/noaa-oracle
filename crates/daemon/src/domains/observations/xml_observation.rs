use serde::{Deserialize, Serialize};

use crate::{DataSource, Request};

#[derive(Serialize, Deserialize)]
#[serde(rename = "response")]
pub struct ObservationData {
    #[serde(rename = "request_index")]
    pub request_index: String,

    #[serde(rename = "data_source")]
    pub data_source: DataSource,

    #[serde(rename = "request")]
    pub request: Request,

    #[serde(rename = "errors")]
    pub errors: String,

    #[serde(rename = "warnings")]
    pub warnings: String,

    #[serde(rename = "time_taken_ms")]
    pub time_taken_ms: String,

    #[serde(rename = "data")]
    pub data: CurrentData,
}

#[derive(Serialize, Deserialize)]
pub struct CurrentData {
    #[serde(rename = "METAR")]
    pub metar: Vec<Metar>,

    #[serde(rename = "num_results")]
    pub num_results: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Metar {
    #[serde(rename = "raw_text")]
    pub raw_text: String,

    #[serde(rename = "station_id")]
    pub station_id: String,

    #[serde(rename = "observation_time")]
    pub observation_time: Option<String>,

    #[serde(rename = "latitude")]
    pub latitude: Option<String>,

    #[serde(rename = "longitude")]
    pub longitude: Option<String>,

    #[serde(rename = "temp_c")]
    pub temp_c: Option<String>,

    #[serde(rename = "dewpoint_c")]
    pub dewpoint_c: Option<String>,

    #[serde(rename = "wind_dir_degrees")]
    pub wind_dir_degrees: Option<String>,

    #[serde(rename = "wind_speed_kt")]
    pub wind_speed_kt: Option<String>,

    #[serde(rename = "elevation_m")]
    pub elevation_m: Option<String>,

    #[serde(rename = "wx_string")]
    pub wx_string: Option<String>,

    #[serde(rename = "precip_in")]
    pub precip_in: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct QualityControlFlags {
    #[serde(rename = "auto")]
    pub auto: Option<String>,

    #[serde(rename = "auto_station")]
    pub auto_station: Option<String>,
    #[serde(rename = "no_signal")]
    pub no_signal: Option<String>,
}
