use crate::filesystem::vfs::FileSystemMakerData;
use alloc::string::String;
use alloc::vec::Vec;
use system_error::SystemError;

pub(super) const OVL_MAX_STACK: usize = 500;

#[derive(Debug)]
pub struct OverlayMountData {
    pub(super) upper_dir: Option<String>,
    pub(super) lower_dirs: Vec<String>,
    pub(super) work_dir: Option<String>,
}

impl OverlayMountData {
    pub fn from_raw(raw_data: Option<&str>) -> Result<Self, SystemError> {
        let raw_str = raw_data.ok_or(SystemError::EINVAL)?;
        if raw_str.is_empty() {
            return Err(SystemError::EINVAL);
        }
        let mut upper_dir = None;
        let mut lower_dirs = None;
        let mut work_dir = None;

        for pair in raw_str.split(',') {
            let (key, value) = pair.split_once('=').ok_or(SystemError::EINVAL)?;
            if key.is_empty() || value.is_empty() {
                return Err(SystemError::EINVAL);
            }

            match key {
                "upperdir" => upper_dir = Some(value.into()),
                "lowerdir" => lower_dirs = Some(Self::parse_lower_dirs(value)?),
                "workdir" => work_dir = Some(value.into()),
                _ => return Err(SystemError::EINVAL),
            }
        }

        let lower_dirs = lower_dirs.ok_or(SystemError::EINVAL)?;
        if (upper_dir.is_none() && lower_dirs.len() < 2)
            || (upper_dir.is_some() && work_dir.is_none())
        {
            return Err(SystemError::EINVAL);
        }

        Ok(OverlayMountData {
            upper_dir,
            lower_dirs,
            work_dir,
        })
    }

    pub(super) fn parse_lower_dirs(raw: &str) -> Result<Vec<String>, SystemError> {
        let mut lower_dirs = Vec::new();
        for dir in raw.split(':') {
            if dir.is_empty() {
                return Err(SystemError::EINVAL);
            }
            if lower_dirs.len() == OVL_MAX_STACK {
                return Err(SystemError::EINVAL);
            }
            lower_dirs.push(dir.into());
        }

        if lower_dirs.is_empty() {
            return Err(SystemError::EINVAL);
        }
        Ok(lower_dirs)
    }
}

impl FileSystemMakerData for OverlayMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
