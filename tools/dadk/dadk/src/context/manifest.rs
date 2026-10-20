use std::{path::PathBuf, str::FromStr};

use crate::utils::abs_path;

use super::DADKExecContextBuilder;
use anyhow::{anyhow, Result};
use dadk_config::manifest::DadkManifestFile;

pub(super) fn parse_manifest(builder: &mut DADKExecContextBuilder) -> Result<()> {
    let manifest_path = PathBuf::from_str(&builder.command.as_ref().unwrap().manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to get manifest path: {}", e))?;

    let workdir = builder.command.as_ref().unwrap().workdir.clone();

    // 将相对路径转换为基于workdir的绝对路径
    let manifest_path = abs_path(&PathBuf::from(workdir)).join(manifest_path);

    if !manifest_path.exists() || !manifest_path.is_file() {
        return Err(anyhow!("Manifest path does not exist or is not a file"));
    }
    let dadk_manifest_file = DadkManifestFile::load(&manifest_path)?;
    builder.manifest = Some(Some(dadk_manifest_file));
    Ok(())
}
