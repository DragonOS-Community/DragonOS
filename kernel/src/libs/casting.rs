// Copyright (C) DragonOS Community  longjin 2023

// This program is free software; you can redistribute it and/or
// modify it under the terms of the GNU General Public License
// as published by the Free Software Foundation; either version 2
// of the License, or (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program; if not, write to the Free Software
// Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
// Or you can visit https://www.gnu.org/licenses/gpl-2.0.html
#![allow(dead_code)]

use core::any::Any;

use alloc::sync::Arc;

/// @brief 将Arc<dyn xxx>转换为Arc<具体类型>的trait
///
/// 用法：
///
/// ```rust
/// trait Base: Any + Send + Sync + Debug {
///     fn get_name(&self) -> String;
/// }
///
/// struct A {
///    name: String,
/// }
///
/// impl DowncastArc for dyn Base {
///     fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
///         return self;
///     }
/// }
///
/// impl Base for A {
///    fn get_name(&self) -> String {
///       return self.name.clone();
///   }
/// }
///
/// fn test() {
///     let a = A { name: "a".to_string() };

///     let a_arc: Arc<dyn Base> = Arc::new(a) as Arc<dyn Base>;
///     let a_arc2: Option<Arc<A>> = a_arc.downcast_arc::<A>();
///     assert!(a_arc2.is_some());
/// }
/// ```
pub trait DowncastArc: Any + Send + Sync {
    /// 请在具体类型中实现这个函数，返回self
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any>;

    /// @brief 将Arc<dyn xxx>转换为Arc<具体类型>
    ///
    /// 如果Arc<dyn xxx>是Arc<具体类型>，则返回Some(Arc<具体类型>)，否则返回None
    ///
    /// @param self Arc<dyn xxx>
    fn downcast_arc<T: Any + Send + Sync>(self: Arc<Self>) -> Option<Arc<T>> {
        let x: Arc<dyn Any> = self.as_any_arc();
        if x.is::<T>() {
            // into_raw不会改变引用计数
            let p = Arc::into_raw(x);
            let new = unsafe { Arc::from_raw(p as *const T) };
            return Some(new);
        }
        return None;
    }
}
