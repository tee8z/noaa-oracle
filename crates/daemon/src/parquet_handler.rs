use std::{fs::File, sync::Arc};

#[cfg(feature = "s3")]
use std::path::Path;

use anyhow::{anyhow, Error};
use parquet::{
    file::{properties::WriterProperties, writer::SerializedFileWriter},
    record::RecordWriter,
};
use reqwest::{multipart, Body, Client};
use slog::{error, info, Logger};
use tokio::fs::File as TokioFile;
use tokio_util::codec::{BytesCodec, FramedRead};

use crate::{
    create_forecast_schema, create_observation_schema, get_full_path, Cli, Forecast, Observation,
};

#[cfg(feature = "s3")]
use crate::S3Storage;

pub fn save_observations(
    observations: Vec<Observation>,
    root_path: &str,
    file_name: String,
) -> String {
    let full_name = format!("{}/{}.parquet", root_path, file_name);

    let file = File::create(full_name.clone()).unwrap();
    let props = WriterProperties::builder().build();
    let mut writer =
        SerializedFileWriter::new(file, Arc::new(create_observation_schema()), Arc::new(props))
            .unwrap();

    let mut row_group = writer.next_row_group().unwrap();
    observations
        .as_slice()
        .write_to_row_group(&mut row_group)
        .unwrap();
    row_group.close().unwrap();
    writer.close().unwrap();
    full_name
}

pub fn save_forecasts(forecast: Vec<Forecast>, root_path: &str, file_name: String) -> String {
    let full_name = format!("{}/{}.parquet", root_path, file_name);
    let file = File::create(full_name.clone()).unwrap();

    let props = WriterProperties::builder().build();
    let mut writer =
        SerializedFileWriter::new(file, Arc::new(create_forecast_schema()), Arc::new(props))
            .unwrap();

    let mut row_group = writer.next_row_group().unwrap();
    forecast
        .as_slice()
        .write_to_row_group(&mut row_group)
        .unwrap();
    row_group.close().unwrap();
    writer.close().unwrap();
    full_name
}

#[cfg(feature = "s3")]
pub async fn upload_to_s3(
    s3: &S3Storage,
    logger: &Logger,
    observation_path: &str,
    forecast_path: &str,
    date_folder: &str,
) -> Result<(), Error> {
    let obs_filename = Path::new(observation_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("invalid observation path"))?;

    let forecast_filename = Path::new(forecast_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("invalid forecast path"))?;

    s3.upload_parquet(Path::new(observation_path), date_folder, obs_filename)
        .await?;

    s3.upload_parquet(Path::new(forecast_path), date_folder, forecast_filename)
        .await?;

    info!(logger, "Uploaded parquet files to S3");
    Ok(())
}

pub async fn send_parquet_files(
    cli: &Cli,
    logger: &Logger,
    observation_relative_file_path: String,
    forecast_relative_file_path_file: String,
) -> Result<(), Error> {
    let base_url = cli
        .base_url
        .clone()
        .unwrap_or(String::from("http://localhost:9100"));
    let observation_filename = observation_relative_file_path
        .split('/')
        .next_back()
        .unwrap();
    let forecast_filename = forecast_relative_file_path_file
        .split('/')
        .next_back()
        .unwrap();

    let observation_full_path = get_full_path(observation_relative_file_path.clone());
    let forecast_full_path = get_full_path(forecast_relative_file_path_file.clone());

    let url_observ = format!("{}/file/{}", base_url, observation_filename);
    let url_forcast = format!("{}/file/{}", base_url, forecast_filename);

    match send_file_to_endpoint(
        logger,
        &observation_full_path,
        observation_filename,
        &url_observ,
    )
    .await
    {
        Ok(_) => {}
        Err(e) => {
            error!(logger, "failed to upload observations: {}", e)
        }
    }
    match send_file_to_endpoint(logger, &forecast_full_path, forecast_filename, &url_forcast).await
    {
        Ok(_) => {}
        Err(e) => {
            error!(logger, "failed to upload forecasts: {}", e)
        }
    }
    Ok(())
}

async fn send_file_to_endpoint(
    logger: &Logger,
    file_path: &str,
    file_name: &str,
    endpoint_url: &str,
) -> Result<(), anyhow::Error> {
    let client = Client::new();

    let file = TokioFile::open(file_path)
        .await
        .map_err(|e| anyhow!("error opening file to upload: {}", e))?;

    let stream = FramedRead::new(file, BytesCodec::new());
    let file_body = Body::wrap_stream(stream);

    let parquet_file = multipart::Part::stream(file_body)
        .file_name(file_name.to_owned())
        .mime_str("application/parquet")?;

    let form = multipart::Form::new().part("file", parquet_file);

    info!(logger, "sending file to endpoint: {}", endpoint_url);
    let response = client
        .post(endpoint_url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| anyhow!("error sending file to api: {}", e))?;

    if response.status().is_success() {
        info!(logger, "file successfully uploaded.");
    } else {
        error!(
            logger,
            "failed to upload the file. status code: {:?}",
            response.status()
        );
    }

    Ok(())
}
