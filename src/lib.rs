use std::cell::RefCell;
use std::collections::HashMap;
use thiserror::Error;
use x11rb::connection::Connection;
use x11rb::protocol::xproto;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::xcb_ffi::{ConnectError, ConnectionError, ReplyError, XCBConnection};
use xkbcommon::xkb::KeyDirection;

struct KeymapState {
    mapping: xkbcommon::xkb::Keymap,
    // Which keycode activate which modifier, assuming modifiers are independent.
    modifier_keycode: HashMap<u8, u32>,
}

pub struct InputSynth {
    connection: XCBConnection,
    screen: usize,
    mapping: RefCell<KeymapState>,
    xkb_context: xkbcommon::xkb::Context,
}

unsafe impl Send for InputSynth {}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Connect(#[from] ConnectError),
    #[error("{0}")]
    Connection(#[from] ConnectionError),
    #[error("{0}")]
    Reply(#[from] ReplyError),
}

extern "C" {
    fn xkb_keymap_key_get_mods_for_level(
        _: *mut xkbcommon::xkb::ffi::xkb_keymap,
        _: xkbcommon::xkb::ffi::xkb_keycode_t,
        _: xkbcommon::xkb::ffi::xkb_layout_index_t,
        _: xkbcommon::xkb::ffi::xkb_level_index_t,
        _: *mut xkbcommon::xkb::ffi::xkb_mod_mask_t,
        _: usize,
    ) -> usize;
}
type Result<T, E = Error> = std::result::Result<T, E>;
mod xkb_extra {
    use xkbcommon::xkb::{
        x11::ffi::{xkb_x11_keymap_new_from_device, xkb_x11_state_new_from_device},
        Context, Keymap, KeymapCompileFlags, State,
    };

    #[must_use]
    pub(super) fn keymap_new_from_device(
        context: &Context,
        connection: &x11rb::xcb_ffi::XCBConnection,
        device_id: i32,
        flags: KeymapCompileFlags,
    ) -> Keymap {
        unsafe {
            Keymap::from_raw_ptr(xkb_x11_keymap_new_from_device(
                context.get_raw_ptr(),
                connection.get_raw_xcb_connection() as *mut _,
                device_id,
                flags,
            ))
        }
    }

    #[must_use]
    pub(super) fn state_new_from_device(
        keymap: &Keymap,
        connection: &x11rb::xcb_ffi::XCBConnection,
        device_id: i32,
    ) -> State {
        unsafe {
            State::from_raw_ptr(xkb_x11_state_new_from_device(
                keymap.get_raw_ptr(),
                connection.get_raw_xcb_connection() as *mut _,
                device_id,
            ))
        }
    }
}
impl InputSynth {
    pub fn new() -> Result<Self> {
        let (connection, screen) = XCBConnection::connect(None)?;
        let (xkb_major, xkb_minor) = x11rb::protocol::xkb::X11_XML_VERSION;
        x11rb::protocol::xkb::use_extension(&connection, xkb_major as _, xkb_minor as _)?
            .reply()?;

        let (xtest_major, xtest_minor) = x11rb::protocol::xtest::X11_XML_VERSION;
        connection
            .xtest_get_version(xtest_major as _, xtest_minor as _)?
            .reply()?;
        let context = xkbcommon::xkb::Context::new(0);

        connection.flush()?;
        Ok(Self {
            mapping: RefCell::new(Self::get_keymap_state(&connection, &context)?),
            connection,
            screen,
            xkb_context: context,
        })
    }

    fn get_keymap_state(
        conn: &XCBConnection,
        ctx: &xkbcommon::xkb::Context,
    ) -> Result<KeymapState> {
        conn.flush()?;
        let devices = x11rb::protocol::xinput::list_input_devices(conn)?.reply()?;
        let device = devices
            .devices
            .iter()
            .find(|d| d.device_use == x11rb::protocol::xinput::DeviceUse::IS_X_KEYBOARD)
            .unwrap();
        let mapping = xkb_extra::keymap_new_from_device(ctx, conn, device.device_id as _, 0);
        let mut state = xkb_extra::state_new_from_device(&mapping, conn, device.device_id as _);

        let mut modifier_keycode = HashMap::new();
        mapping.key_for_each(|map, k| {
            // reset mask
            state.update_mask(0, 0, 0, 0, 0, 0);
            state.update_key(k, KeyDirection::Down);
            for m in 0..map.num_mods() {
                if state.mod_index_is_active(m, xkbcommon::xkb::STATE_MODS_DEPRESSED) {
                    modifier_keycode.insert(m as u8, k);
                }
            }
        });

        Ok(KeymapState {
            mapping,
            modifier_keycode,
        })
    }

    fn handle_events(&self) -> Result<()> {
        while let Some(event) = self.connection.poll_for_event()? {
            use x11rb::protocol::Event;
            if let Event::MappingNotify(_) = event {
                self.mapping
                    .replace(Self::get_keymap_state(&self.connection, &self.xkb_context)?);
            }
        }
        Ok(())
    }

    /// Generate a mouse click at `(x, y)`, with `button`. `press` indicates if the click is a
    /// press, if it's false, a release will be generated.
    pub fn click(&self, x: i16, y: i16, button: u8, press: bool) -> Result<()> {
        self.handle_events()?;
        self.connection
            .xtest_fake_input(
                if press {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                },
                button,
                x11rb::CURRENT_TIME,
                self.connection.setup().roots[self.screen].root,
                x,
                y,
                x11rb::NONE as _,
            )?
            .check()?;
        Ok(())
    }
    pub fn move_cursor(&self, x: i16, y: i16) -> Result<()> {
        self.handle_events()?;
        self.connection
            .xtest_fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                0,
                x11rb::CURRENT_TIME,
                self.connection.setup().roots[self.screen].root,
                x,
                y,
                x11rb::NONE as _,
            )?
            .check()?;
        Ok(())
    }

    pub(crate) fn find_key_sequence(&self, sym: u16) -> Option<(Vec<u32>, u32)> {
        // TODO: handle layouts, now we always assume layout 0
        let mapping = self.mapping.borrow();
        let mut ans = None;
        mapping.mapping.key_for_each(|map, k| {
            if ans.is_none() {
                let nlevels = map.num_levels_for_key(k, 0);
                for level in 0..nlevels {
                    let syms = map.key_get_syms_by_level(k, 0, level);
                    if syms.len() == 1 && syms[0] == sym.into() {
                        ans.replace((level, k));
                    }
                }
            }
        });

        // Get the key sequence that will produce level + keycode
        let mut mods = Vec::new();
        if let Some((level, keycode)) = ans {
            let mut masks = [0; 4];
            unsafe {
                xkb_keymap_key_get_mods_for_level(
                    mapping.mapping.get_raw_ptr(),
                    keycode,
                    0,
                    level,
                    masks.as_mut_ptr(),
                    4,
                )
            };
            'next_mask: for mask in masks.iter() {
                for m in 0..mapping.mapping.num_mods() {
                    if (*mask & (1 << m)) != 0 && !mapping.modifier_keycode.contains_key(&(m as _))
                    {
                        continue 'next_mask;
                    }
                }
                // We are able to find all the modifiers
                for m in 0..mapping.mapping.num_mods() {
                    if (*mask & (1 << m)) != 0 {
                        mods.push(*mapping.modifier_keycode.get(&(m as _)).unwrap())
                    }
                }
                return Some((mods, keycode));
            }
        }
        None
    }

    pub fn ascii_char(&self, ch: u8) -> Result<()> {
        self.handle_events()?;
        let mut keysym: u16 = ch as _;
        if (8..=17).contains(&ch) {
            // Function keysyms are encoded in X as 0xffxx,
            // we cover the most often used ones here.
            keysym += 0xff00;
        }

        if let Some((mods, keycode)) = self.find_key_sequence(keysym) {
            for &m in &mods {
                self.connection.xtest_fake_input(
                    xproto::KEY_PRESS_EVENT,
                    m as _,
                    x11rb::CURRENT_TIME,
                    self.connection.setup().roots[self.screen].root,
                    0,
                    0,
                    x11rb::NONE as _,
                )?;
            }
            self.connection.xtest_fake_input(
                xproto::KEY_PRESS_EVENT,
                keycode as _,
                x11rb::CURRENT_TIME,
                self.connection.setup().roots[self.screen].root,
                0,
                0,
                x11rb::NONE as _,
            )?;
            self.connection.xtest_fake_input(
                xproto::KEY_RELEASE_EVENT,
                keycode as _,
                x11rb::CURRENT_TIME,
                self.connection.setup().roots[self.screen].root,
                0,
                0,
                x11rb::NONE as _,
            )?;
            for &m in mods.iter().rev() {
                self.connection.xtest_fake_input(
                    xproto::KEY_RELEASE_EVENT,
                    m as _,
                    x11rb::CURRENT_TIME,
                    self.connection.setup().roots[self.screen].root,
                    0,
                    0,
                    x11rb::NONE as _,
                )?;
            }
            self.connection.flush()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_find_key_sequence() {
        let is = super::InputSynth::new().unwrap();
        let (mods, keycode) = is.find_key_sequence(b'A' as _).unwrap();
        println!("{mods:?} {keycode}");
    }
}
