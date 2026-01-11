#[cfg(feature = "s3")]
use aws_sdk_s3::Client;
#[cfg(feature = "s3")]
use slog::{error, info, Logger};
#[cfg(feature = "s3")]
use std::path::Path;

#[cfg(feature = "s3")]
pub struct S3Storage {
    client: Client,
    bucket: String,
    logger: Logger,
}

#[cfg(feature = "s3")]
impl S3Storage {
    pub async fn new(
        bucket: String,
        endpoint: Option<String>,
        logger: Logger,
    ) -> Result<Self, anyhow::Error> {
        let mut config_loader = aws_config::from_env();

        if let Some(endpoint_url) = endpoint {
            info!(logger, "Using custom S3 endpoint: {}", endpoint_url);
            config_loader = config_loader.endpoint_url(endpoint_url);
        }

        let config = config_loader.load().await;
        let client = Client::new(&config);

        info!(logger, "S3 storage initialized for bucket: {}", bucket);

        Ok(Self {
            client,
            bucket,
            logger,
        })
    }

    pub async fn upload_file(&self, local_path: &Path, s3_key: &str) -> Result<(), anyhow::Error> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(local_path).await?;

        info!(
            self.logger,
            "Uploading {} to s3://{}/{}",
            local_path.display(),
            self.bucket,
            s3_key
        );

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(s3_key)
            .body(body)
            .content_type("application/parquet")
            .send()
            .await
            .map_err(|e| {
                error!(self.logger, "Failed to upload to S3: {}", e);
                anyhow::anyhow!("S3 upload failed: {}", e)
            })?;

        info!(
            self.logger,
            "Successfully uploaded to s3://{}/{}", self.bucket, s3_key
        );

        Ok(())
    }

    pub async fn upload_parquet(
        &self,
        local_path: &Path,
        date_folder: &str,
        filename: &str,
    ) -> Result<(), anyhow::Error> {
        let s3_key = format!("weather_data/{}/{}", date_folder, filename);
        self.upload_file(local_path, &s3_key).await
    }
}

#[cfg(not(feature = "s3"))]
pub struct S3Storage;

#[cfg(not(feature = "s3"))]
impl S3Storage {
    pub async fn new(
        _bucket: String,
        _endpoint: Option<String>,
        _logger: slog::Logger,
    ) -> Result<Self, anyhow::Error> {
        Err(anyhow::anyhow!(
            "S3 storage requires the 's3' feature to be enabled"
        ))
    }
}
