use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::tty::{
    console::ConsoleSwitch,
    termios::WindowSize,
    tty_driver::TtyOperation,
    virtual_terminal::{
        virtual_console::{CursorOperation, ScrollDir, VirtualConsoleData},
        VirtConsole,
    },
};

lazy_static! {
    pub static ref DUMMY_CONSOLE: Arc<DummyConsole> = Arc::new(DummyConsole::new());
}

#[inline]
#[allow(dead_code)]
pub fn dummy_console() -> Arc<DummyConsole> {
    DUMMY_CONSOLE.clone()
}

pub struct DummyConsole;

impl DummyConsole {
    pub const COLUNMS: usize = 80;
    pub const ROWS: usize = 25;
    pub const fn new() -> Self {
        DummyConsole
    }
}

impl ConsoleSwitch for DummyConsole {
    fn con_getxy(
        &self,
        _vc_data: &VirtualConsoleData,
        _pos: usize,
    ) -> Result<(usize, usize, usize), SystemError> {
        Ok((0, 0, 0))
    }

    fn con_build_attr(
        &self,
        _vc_data: &VirtualConsoleData,
        _color: u8,
        _intensity: crate::driver::tty::virtual_terminal::virtual_console::VirtualConsoleIntensity,
        _blink: bool,
        _underline: bool,
        _reverse: bool,
        _italic: bool,
    ) -> Result<u8, SystemError> {
        Ok(0)
    }
    fn con_init(
        &self,
        vc: &Arc<VirtConsole>,
        vc_data: &mut VirtualConsoleData,
        init: bool,
    ) -> Result<(), SystemError> {
        vc_data.color_mode = true;

        if init {
            vc_data.cols = Self::COLUNMS;
            vc_data.rows = Self::ROWS;
        } else {
            let tty = vc.port().port_data().tty().unwrap();
            tty.resize(
                tty.clone(),
                WindowSize::new(Self::ROWS as u16, Self::COLUNMS as u16, 0, 0),
            )?;
        }

        Ok(())
    }

    fn con_deinit(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn con_clear(
        &self,
        _vc_data: &mut VirtualConsoleData,
        _sy: usize,
        _sx: usize,
        _height: usize,
        _width: usize,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn con_putc(
        &self,
        _vc_data: &VirtualConsoleData,
        _ch: u16,
        _ypos: u32,
        _xpos: u32,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn con_putcs(
        &self,
        _vc_data: &VirtualConsoleData,
        _buf: &[u16],
        _count: usize,
        _ypos: u32,
        _xpos: u32,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn con_cursor(&self, _vc_data: &VirtualConsoleData, _op: CursorOperation) {
        // Do nothing
    }

    fn con_set_palette(
        &self,
        _vc_data: &VirtualConsoleData,
        _color_table: &[u8],
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn con_scroll(
        &self,
        _vc_data: &mut VirtualConsoleData,
        _top: usize,
        _bottom: usize,
        _dir: ScrollDir,
        _nr: usize,
    ) -> bool {
        false
    }
}
