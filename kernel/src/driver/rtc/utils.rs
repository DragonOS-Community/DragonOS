use alloc::sync::Arc;
use intertrait::cast::CastArc;

use crate::driver::base::kobject::KObject;

use super::{sysfs::RtcGeneralDevice, RtcDevice};

#[inline]
pub fn kobj2rtc_device(kobj: Arc<dyn KObject>) -> Option<Arc<dyn RtcDevice>> {
    kobj.arc_any().cast::<dyn RtcDevice>().ok()
}

#[inline]
pub fn kobj2rtc_general_device(kobj: Arc<dyn KObject>) -> Option<Arc<RtcGeneralDevice>> {
    kobj.arc_any().downcast().ok()
}
