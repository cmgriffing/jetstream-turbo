use aws_config::BehaviorVersion;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use std::path::Path;
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;
use tracing::{debug, error, info};
use crate::models::errors::{TurboError, TurboResult};

pub struct S3Store {
    client: S3Client,
    bucket: String,
    region: String,
}

impl S3Store {
    pub async fn new(bucket: String, region: String) -> TurboResult<Self> {
        info!("Initializing S3 client for bucket: {} in region: {}", bucket, region);
        
        let config = aws_config::defaults(BehaviorVersion::v2024())
            .region(aws_sdk_s3::config::Region::new(region.clone()))
            .load()
            .await;
            
        let client = S3Client::new(&config);
        
        Ok(Self {
            client,
            bucket,
            region,
        })
    }
    
    pub async fn upload_file<P: AsRef<Path>>(&self, local_path: P, s3_key: &str) -> TurboResult<()> {
        let local_path = local_path.as_ref();
        
        info!("Uploading {} to s3://{}/{}", local_path.display(), self.bucket, s3_key);
        
        let body = ByteStream::from_path(local_path).await
            .map_err(|e| TurboError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(s3_key)
            .body(body)
            .send()
            .await
            .map_err(|e| TurboError::S3Operation(e))?;
        
        debug!("Successfully uploaded {} to S3", local_path.display());
        Ok(())
    }
    
    pub async fn upload_compressed_directory<P: AsRef<Path>>(
        &self, 
        directory: P, 
        s3_key: &str
    ) -> TurboResult<()> {
        let directory = directory.as_ref();
        info!("Compressing and uploading directory: {}", directory.display());
        
        // Create tar.gz in memory
        let mut tar_gz = Vec::new();
        {
            let encoder = GzEncoder::new(&mut tar_gz, Compression::default());
            let mut tar = Builder::new(encoder);
            
            if directory.is_dir() {
                tar.append_dir_all(".", directory)
                    .map_err(|e| TurboError::Io(e))?;
            }
        }
        
        // Upload to S3
        let body = ByteStream::from(tar_gz);
        
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(s3_key)
            .body(body)
            .content_type("application/gzip")
            .send()
            .await
            .map_err(|e| TurboError::S3Operation(e))?;
        
        info!("Successfully uploaded compressed directory to s3://{}/{}", self.bucket, s3_key);
        Ok(())
    }
    
    pub async fn file_exists(&self, s3_key: &str) -> TurboResult<bool> {
        match self.client
            .head_object()
            .bucket(&self.bucket)
            .key(s3_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.is_service_error() {
                    error!("Error checking if S3 file exists: {}", e);
                    Err(TurboError::S3Operation(e))
                } else {
                    // 404 or similar - file doesn't exist
                    Ok(false)
                }
            }
        }
    }
    
    pub async fn delete_file(&self, s3_key: &str) -> TurboResult<()> {
        info!("Deleting s3://{}/{}", self.bucket, s3_key);
        
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(s3_key)
            .send()
            .await
            .map_err(|e| TurboError::S3Operation(e))?;
        
        debug!("Successfully deleted {} from S3", s3_key);
        Ok(())
    }
    
    pub async fn list_files(&self, prefix: &str) -> TurboResult<Vec<String>> {
        let mut files = Vec::new();
        let mut continuation_token: Option<String> = None;
        
        loop {
            let mut request = self.client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
                
            if let Some(token) = &continuation_token {
                request = request.continuation_token(token);
            }
            
            let response = request
                .send()
                .await
                .map_err(|e| TurboError::S3Operation(e))?;
            
            if let Some(objects) = response.contents() {
                for object in objects {
                    if let Some(key) = object.key() {
                        files.push(key.clone());
                    }
                }
            }
            
            continuation_token = response.next_continuation_token().cloned();
            if continuation_token.is_none() {
                break;
            }
        }
        
        debug!("Listed {} files with prefix: {}", files.len(), prefix);
        Ok(files)
    }
    
    pub fn get_bucket(&self) -> &str {
        &self.bucket
    }
    
    pub fn get_region(&self) -> &str {
        &self.region
    }
}