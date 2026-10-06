//! The part of `xkbcommon` 0.9's `xkb` module that xa11y-linux calls, over a
//! libxkbcommon opened at run time. Without the library, contexts are empty
//! and keymaps fail to compile (`None`), which xa11y reports as an error for
//! Wayland keyboard input only.

pub mod xkb {
    use std::{borrow::Borrow, ffi::CString, ptr, slice};

    use xkbcommon_dl::{
        xkb_context, xkb_context_flags, xkb_keymap, xkb_keymap_compile_flags, xkb_rule_names,
        xkbcommon_option, XkbCommon,
    };

    pub use xkeysym::{KeyCode as Keycode, Keysym};

    pub type ContextFlags = xkb_context_flags;
    pub type KeymapCompileFlags = xkb_keymap_compile_flags;
    pub type LayoutIndex = u32;
    pub type LevelIndex = u32;

    pub const CONTEXT_NO_FLAGS: ContextFlags = xkb_context_flags::XKB_CONTEXT_NO_FLAGS;
    pub const KEYMAP_COMPILE_NO_FLAGS: KeymapCompileFlags =
        xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS;

    fn library() -> Option<&'static XkbCommon> {
        xkbcommon_option()
    }

    pub struct Context {
        ptr: *mut xkb_context,
    }

    impl Context {
        /// An empty context when libxkbcommon is missing or out of memory.
        #[must_use]
        pub fn new(flags: ContextFlags) -> Context {
            let ptr = library().map_or(ptr::null_mut(), |xkb| {
                // SAFETY: a plain constructor; null on failure.
                unsafe { (xkb.xkb_context_new)(flags) }
            });
            Context { ptr }
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            if let (false, Some(xkb)) = (self.ptr.is_null(), library()) {
                // SAFETY: ptr is our reference from xkb_context_new.
                unsafe { (xkb.xkb_context_unref)(self.ptr) }
            }
        }
    }

    pub struct Keymap {
        ptr: *mut xkb_keymap,
        xkb: &'static XkbCommon,
    }

    impl Keymap {
        /// Compiles a keymap from RMLVO names; `None` if that fails or
        /// libxkbcommon is missing.
        pub fn new_from_names<S: Borrow<str> + ?Sized>(
            context: &Context,
            rules: &S,
            model: &S,
            layout: &S,
            variant: &S,
            options: Option<String>,
            flags: KeymapCompileFlags,
        ) -> Option<Keymap> {
            let xkb = library()?;
            if context.ptr.is_null() {
                return None;
            }
            let rules = CString::new(rules.borrow()).ok()?;
            let model = CString::new(model.borrow()).ok()?;
            let layout = CString::new(layout.borrow()).ok()?;
            let variant = CString::new(variant.borrow()).ok()?;
            let options = options.map(CString::new).transpose().ok()?;
            let names = xkb_rule_names {
                rules: rules.as_ptr(),
                model: model.as_ptr(),
                layout: layout.as_ptr(),
                variant: variant.as_ptr(),
                options: options.as_ref().map_or(ptr::null(), |options| options.as_ptr()),
            };
            // SAFETY: the context is live and the names outlive the call.
            let ptr = unsafe { (xkb.xkb_keymap_new_from_names)(context.ptr, &names, flags) };
            (!ptr.is_null()).then_some(Keymap { ptr, xkb })
        }

        #[must_use]
        pub fn min_keycode(&self) -> Keycode {
            // SAFETY: self.ptr is a live keymap.
            Keycode::new(unsafe { (self.xkb.xkb_keymap_min_keycode)(self.ptr) })
        }

        #[must_use]
        pub fn max_keycode(&self) -> Keycode {
            // SAFETY: self.ptr is a live keymap.
            Keycode::new(unsafe { (self.xkb.xkb_keymap_max_keycode)(self.ptr) })
        }

        /// The keysyms at a key's shift level, borrowed from the keymap.
        #[must_use]
        pub fn key_get_syms_by_level(
            &self,
            key: Keycode,
            layout: LayoutIndex,
            level: LevelIndex,
        ) -> &[Keysym] {
            let mut syms: *const u32 = ptr::null();
            // SAFETY: self.ptr is a live keymap and syms a valid out pointer.
            let len = unsafe {
                (self.xkb.xkb_keymap_key_get_syms_by_level)(
                    self.ptr, key.raw(), layout, level, &mut syms,
                )
            };
            match usize::try_from(len) {
                Ok(len) if len > 0 && !syms.is_null() => {
                    // SAFETY: libxkbcommon returns `len` keysyms owned by the
                    // keymap, alive as long as `self`; `Keysym` is a
                    // `repr(transparent)` u32.
                    unsafe { slice::from_raw_parts(syms.cast::<Keysym>(), len) }
                }
                _ => &[],
            }
        }
    }

    impl Drop for Keymap {
        fn drop(&mut self) {
            // SAFETY: ptr is our reference from xkb_keymap_new_from_names.
            unsafe { (self.xkb.xkb_keymap_unref)(self.ptr) }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// With libxkbcommon installed, the US keymap maps `a`; without it
        /// everything degrades to `None` instead of failing to start.
        #[test]
        fn compiles_the_us_keymap_when_the_library_is_present() {
            let context = Context::new(CONTEXT_NO_FLAGS);
            let keymap =
                Keymap::new_from_names(&context, "", "", "us", "", None, KEYMAP_COMPILE_NO_FLAGS);
            let Some(keymap) = keymap else {
                assert!(library().is_none(), "libxkbcommon loaded but the keymap failed");
                return;
            };
            let a = xkeysym::key::a;
            let found = (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).any(|code| {
                keymap
                    .key_get_syms_by_level(Keycode::new(code), 0, 0)
                    .iter()
                    .any(|sym| sym.raw() == a)
            });
            assert!(found);
        }
    }
}
