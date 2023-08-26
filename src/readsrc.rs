use std::os::raw::c_void;
use crate::{fail, warn};
use mozjpeg_sys::JERR_BAD_LENGTH;
use mozjpeg_sys::boolean;
use mozjpeg_sys::jpeg_decompress_struct;
use mozjpeg_sys::{JERR_VIRTUAL_BUG, JERR_FILE_READ};
use mozjpeg_sys::{jpeg_common_struct, jpeg_resync_to_restart, jpeg_source_mgr};
use mozjpeg_sys::{JWRN_JPEG_EOF, JPOOL_IMAGE, JPOOL_PERMANENT};
use std::io::{self, Read, BufRead, BufReader};
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_long, c_uint};
use std::ptr::NonNull;
use std::ptr;

#[repr(C)]
pub(crate) struct SourceMgr<R> {
    iface: jpeg_source_mgr,
    to_consume: usize,
    reader: R,
}

impl<R: BufRead> SourceMgr<R> {
    #[inline]
    pub(crate) fn set_src(cinfo: &mut jpeg_decompress_struct, reader: R) -> io::Result<()> {
        if !cinfo.src.is_null() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }

        // Handle first read on Rust's side, to have a chance to report an I/O error without aborting
        let this = Self::new(reader)?;

        // cinfo.common.mem.alloc_small can't guarantee alignment,
        // and `R` could require an unusual one.
        let src = Box::new(this);

        assert_eq!(std::mem::size_of::<*mut c_void>(), std::mem::size_of_val(&src)); // not a fat pointer
        cinfo.src = Box::into_raw(src).cast();

        debug_assert!(unsafe { Self::cast(cinfo); true });
        Ok(())
    }

    #[inline]
    fn new(reader: R) -> io::Result<Self> {
        let mut this = Self {
            iface: jpeg_source_mgr {
                next_input_byte: ptr::null_mut(),
                bytes_in_buffer: 0,
                init_source: Some(Self::init_source),
                fill_input_buffer: Some(Self::fill_input_buffer),
                skip_input_data: Some(Self::skip_input_data),
                resync_to_restart: Some(jpeg_resync_to_restart),
                term_source: Some(Self::term_source),
            },
            to_consume: 0,
            reader,
        };
        this.fill_input_buffer_impl()?;
        Ok(this)
    }

    #[inline]
    unsafe fn cast(cinfo: &mut jpeg_decompress_struct) -> &mut Self {
        let this: &mut Self = &mut *cinfo.src.cast();
        // Type alias to unify higher-ranked lifetimes
        type FnPtr<'a> = unsafe extern "C-unwind" fn(cinfo: &'a mut jpeg_decompress_struct);
        // This is a redundant safety check to ensure the struct is ours
        if Some::<FnPtr>(Self::init_source) != this.iface.init_source {
            fail(&mut cinfo.common, JERR_VIRTUAL_BUG);
        }
        this
    }

    unsafe extern "C-unwind" fn init_source(cinfo: &mut jpeg_decompress_struct) {
        // Do nothing, buffer has been filled by new()
        let _s = Self::cast(cinfo);
        debug_assert!(!_s.iface.next_input_byte.is_null());
        debug_assert!(_s.iface.bytes_in_buffer > 0);
    }

    #[cold]
    fn set_buffer_to_eoi(&mut self) {
        // libjpeg doesn't treat it as error, but fakes it!
        self.iface.next_input_byte = [0xFF, 0xD9, 0xFF, 0xD9].as_ptr();
        self.iface.bytes_in_buffer = 4;
    }

    #[inline(never)]
    fn fill_input_buffer_impl(&mut self) -> io::Result<()> {
        // here bytes_in_buffer may be != 0, because jdhuff.c doesn't update
        // the value after consuming the buffer.

        self.reader.consume(self.to_consume);
        let buf = self.reader.fill_buf()?;

        self.iface.next_input_byte = buf.as_ptr();
        self.iface.bytes_in_buffer = buf.len() as _;
        self.to_consume = buf.len();

        if buf.is_empty() {
            // this is EOF
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }

    /// In typical applications, it should read fresh data
    ///    into the buffer (ignoring the current state of next_input_byte and
    ///    bytes_in_buffer)
    unsafe extern "C-unwind" fn fill_input_buffer(cinfo: &mut jpeg_decompress_struct) -> boolean {
        let this = Self::cast(cinfo);
        match this.fill_input_buffer_impl() {
            Ok(()) => 1,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                this.set_buffer_to_eoi();
                warn(&mut cinfo.common, JWRN_JPEG_EOF);
                // boolean returned by this function is for async I/O, not errors.
                1
            },
            Err(_) => {
                // libjpeg won't call term_source itself if fail unwinds
                Self::term_source(cinfo);
                fail(&mut cinfo.common, JERR_FILE_READ);
            }
        }
    }

    unsafe extern "C-unwind" fn skip_input_data(cinfo: &mut jpeg_decompress_struct, num_bytes: c_long) {
        if num_bytes <= 0 {
            return;
        }
        let this = Self::cast(cinfo);
        let mut num_bytes = num_bytes as usize;

        loop {
            if this.iface.bytes_in_buffer > 0 {
                let skip_from_buffer = this.iface.bytes_in_buffer.min(num_bytes);
                this.iface.bytes_in_buffer -= skip_from_buffer;
                this.iface.next_input_byte = this.iface.next_input_byte.add(skip_from_buffer);
                num_bytes -= skip_from_buffer;
            }
            if num_bytes == 0 {
                break;
            }
            if let Err(_) = this.fill_input_buffer_impl() {
                Self::term_source(cinfo);
                fail(&mut cinfo.common, JERR_FILE_READ);
            }
        }
    }

    unsafe extern "C-unwind" fn term_source(cinfo: &mut jpeg_decompress_struct) {
        let _ = Self::cast(cinfo); // checks
        let ptr: *mut Self = cinfo.src.cast();
        cinfo.src = ptr::null_mut();
        if !ptr.is_null() {
            drop(Box::from_raw(ptr)); // user-supplied reader drops here
        }
    }
}
