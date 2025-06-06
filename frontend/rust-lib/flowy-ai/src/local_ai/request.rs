use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Client, Response, StatusCode};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use tokio_util::sync::CancellationToken;
use tracing::{instrument, trace};

#[allow(dead_code)]
type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[instrument(level = "trace", skip_all, err)]
pub async fn download_model(
  url: &str,
  model_path: &Path,
  model_filename: &str,
  progress_callback: Option<ProgressCallback>,
  cancel_token: Option<CancellationToken>,
) -> Result<PathBuf, anyhow::Error> {
  let client = Client::new();
  let mut response = make_request(&client, url, None).await?;
  let total_size_in_bytes = response.content_length().unwrap_or(0);
  let partial_path = model_path.join(format!("{}.part", model_filename));
  let download_path = model_path.join(model_filename);
  let mut part_file = File::create(&partial_path).await?;
  let mut downloaded: u64 = 0;

  let debounce_duration = Duration::from_millis(100);
  let mut last_update = Instant::now()
    .checked_sub(debounce_duration)
    .unwrap_or(Instant::now());

  while let Some(chunk) = response.chunk().await? {
    if let Some(cancel_token) = &cancel_token {
      if cancel_token.is_cancelled() {
        trace!("Download canceled by client");
        fs::remove_file(&partial_path).await?;
        return Err(anyhow!("Download canceled"));
      }
    }

    part_file.write_all(&chunk).await?;
    downloaded += chunk.len() as u64;

    if let Some(progress_callback) = &progress_callback {
      let now = Instant::now();
      if now.duration_since(last_update) >= debounce_duration {
        progress_callback(downloaded, total_size_in_bytes);
        last_update = now;
      }
    }
  }

  // Verify file integrity
  let header_sha256 = response
    .headers()
    .get("SHA256")
    .and_then(|value| value.to_str().ok())
    .and_then(|value| STANDARD.decode(value).ok());

  part_file.seek(tokio::io::SeekFrom::Start(0)).await?;
  let mut hasher = Sha256::new();
  let block_size = 2_usize.pow(20); // 1 MB
  let mut buffer = vec![0; block_size];
  while let Ok(bytes_read) = part_file.read(&mut buffer).await {
    if bytes_read == 0 {
      break;
    }
    hasher.update(&buffer[..bytes_read]);
  }
  let calculated_sha256 = hasher.finalize();
  if let Some(header_sha256) = header_sha256 {
    if calculated_sha256.as_slice() != header_sha256.as_slice() {
      trace!(
        "Header Sha256: {:?}, calculated Sha256:{:?}",
        header_sha256, calculated_sha256
      );

      fs::remove_file(&partial_path).await?;
      return Err(anyhow!(
        "Sha256 mismatch: expected {:?}, got {:?}",
        header_sha256,
        calculated_sha256
      ));
    }
  }

  fs::rename(&partial_path, &download_path).await?;
  Ok(download_path)
}

#[allow(dead_code)]
async fn make_request(
  client: &Client,
  url: &str,
  offset: Option<u64>,
) -> Result<Response, anyhow::Error> {
  let mut request = client.get(url);
  if let Some(offset) = offset {
    println!(
      "\nDownload interrupted, resuming from byte position {}",
      offset
    );
    request = request.header("Range", format!("bytes={}-", offset));
  }
  let response = request.send().await?;
  if !(response.status().is_success() || response.status() == StatusCode::PARTIAL_CONTENT) {
    return Err(anyhow!(response.text().await?));
  }
  Ok(response)
}
