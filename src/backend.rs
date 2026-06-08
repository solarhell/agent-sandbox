#![allow(dead_code)]

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use async_trait::async_trait;

pub type DynObjectBackend = Arc<dyn ObjectBackend>;

#[async_trait]
pub trait ObjectBackend: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
    fn kind(&self) -> BackendKind;
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BackendKind {
    Local,
    AliyunOss,
    AwsS3,
}

#[derive(Debug, Clone)]
pub enum BackendConfig {
    Local {
        root: PathBuf,
    },
    AliyunOss {
        bucket: String,
        endpoint: String,
        prefix: String,
    },
    AwsS3 {
        bucket: String,
        region: String,
        endpoint: Option<String>,
        prefix: String,
    },
}

pub fn build_backend(config: BackendConfig) -> anyhow::Result<DynObjectBackend> {
    match config {
        BackendConfig::Local { root } => Ok(Arc::new(LocalObjectBackend::new(root))),
        BackendConfig::AliyunOss {
            bucket,
            endpoint,
            prefix,
        } => Ok(Arc::new(UnsupportedCloudBackend::aliyun_oss(
            bucket, endpoint, prefix,
        ))),
        BackendConfig::AwsS3 {
            bucket,
            region,
            endpoint,
            prefix,
        } => Ok(Arc::new(UnsupportedCloudBackend::aws_s3(
            bucket, region, endpoint, prefix,
        ))),
    }
}

#[derive(Debug, Clone)]
pub struct LocalObjectBackend {
    root: PathBuf,
}

impl LocalObjectBackend {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path_for_key(&self, key: &str) -> anyhow::Result<PathBuf> {
        let key = normalize_object_key(key)?;
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl ObjectBackend for LocalObjectBackend {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let path = self.path_for_key(key)?;
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("failed to read object `{key}` from {}", path.display()))
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let path = self.path_for_key(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("failed to write object `{key}` to {}", path.display()))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.path_for_key(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to delete object `{key}` at {}", path.display())),
        }
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }
}

#[derive(Debug, Clone)]
pub struct UnsupportedCloudBackend {
    kind: BackendKind,
    bucket: String,
    endpoint: Option<String>,
    region: Option<String>,
    prefix: String,
}

impl UnsupportedCloudBackend {
    pub fn aliyun_oss(bucket: String, endpoint: String, prefix: String) -> Self {
        Self {
            kind: BackendKind::AliyunOss,
            bucket,
            endpoint: Some(endpoint),
            region: None,
            prefix,
        }
    }

    pub fn aws_s3(
        bucket: String,
        region: String,
        endpoint: Option<String>,
        prefix: String,
    ) -> Self {
        Self {
            kind: BackendKind::AwsS3,
            bucket,
            endpoint,
            region: Some(region),
            prefix,
        }
    }

    fn unsupported(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "{:?} backend adapter is configured but not implemented yet; local backend is the only supported backend in this MVP",
            self.kind
        )
    }
}

#[async_trait]
impl ObjectBackend for UnsupportedCloudBackend {
    async fn get(&self, _key: &str) -> anyhow::Result<Vec<u8>> {
        Err(self.unsupported())
    }

    async fn put(&self, _key: &str, _bytes: &[u8]) -> anyhow::Result<()> {
        Err(self.unsupported())
    }

    async fn delete(&self, _key: &str) -> anyhow::Result<()> {
        Err(self.unsupported())
    }

    fn kind(&self) -> BackendKind {
        self.kind
    }
}

fn normalize_object_key(key: &str) -> anyhow::Result<PathBuf> {
    let key = key.trim_start_matches('/');
    if key.is_empty() {
        bail!("object key cannot be empty");
    }

    let mut normalized = PathBuf::new();
    for component in std::path::Path::new(key).components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                bail!("object key `{key}` escapes backend root");
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("object key cannot be empty");
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_backend_round_trips_and_deletes_object() {
        let root = std::env::temp_dir().join(format!(
            "agent-sandbox-local-backend-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = LocalObjectBackend::new(root.clone());
        backend.put("objects/a.txt", b"hello").await.unwrap();
        assert_eq!(backend.get("objects/a.txt").await.unwrap(), b"hello");
        backend.delete("objects/a.txt").await.unwrap();
        assert!(backend.get("objects/a.txt").await.is_err());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn local_backend_rejects_parent_paths() {
        let backend = LocalObjectBackend::new(std::env::temp_dir());
        let err = backend.put("../escape", b"nope").await.unwrap_err();
        assert!(err.to_string().contains("escapes backend root"));
    }

    #[test]
    fn factory_preserves_future_adapter_slots() {
        let backend = build_backend(BackendConfig::AliyunOss {
            bucket: "bucket".to_string(),
            endpoint: "oss-cn-hangzhou.aliyuncs.com".to_string(),
            prefix: "agent-sandbox".to_string(),
        })
        .unwrap();
        assert_eq!(backend.kind(), BackendKind::AliyunOss);
    }
}
