// Разделяемая гостевая память для vhost-user: memfd + mmap(MAP_SHARED).
// Бэкенд мапит тот же fd через SET_MEM_TABLE, поэтому байты, которые мы пишем
// здесь, он видит напрямую (и наоборот). Все чтения/записи — volatile, т.к.
// вторая сторона (демон) правит эту память из другого процесса.

use std::os::unix::io::RawFd;

pub struct SharedMem {
    fd: RawFd,
    base: *mut u8,
    size: usize,
}

impl SharedMem {
    pub fn new(size: usize) -> Result<SharedMem, String> {
        unsafe {
            let name = b"vhost-blk-conformance\0";
            let fd = libc::memfd_create(name.as_ptr() as *const libc::c_char, 0);
            if fd < 0 {
                return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
            }
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("ftruncate: {}", e));
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("mmap: {}", e));
            }
            std::ptr::write_bytes(p as *mut u8, 0, size);
            Ok(SharedMem { fd, base: p as *mut u8, size })
        }
    }

    pub fn fd(&self) -> RawFd { self.fd }
    pub fn base_va(&self) -> u64 { self.base as u64 }
    pub fn size(&self) -> usize { self.size }

    #[inline]
    fn at(&self, off: usize) -> *mut u8 {
        assert!(off <= self.size, "offset {} > region {}", off, self.size);
        unsafe { self.base.add(off) }
    }

    pub fn wr(&self, off: usize, data: &[u8]) {
        assert!(off + data.len() <= self.size);
        let p = self.at(off);
        unsafe {
            for (i, b) in data.iter().enumerate() {
                std::ptr::write_volatile(p.add(i), *b);
            }
        }
    }

    pub fn rd(&self, off: usize, out: &mut [u8]) {
        assert!(off + out.len() <= self.size);
        let p = self.at(off) as *const u8;
        unsafe {
            for (i, b) in out.iter_mut().enumerate() {
                *b = std::ptr::read_volatile(p.add(i));
            }
        }
    }

    pub fn zero(&self, off: usize, len: usize) {
        assert!(off + len <= self.size);
        let p = self.at(off);
        unsafe {
            for i in 0..len {
                std::ptr::write_volatile(p.add(i), 0u8);
            }
        }
    }

    pub fn fill(&self, off: usize, byte: u8, len: usize) {
        assert!(off + len <= self.size);
        let p = self.at(off);
        unsafe {
            for i in 0..len {
                std::ptr::write_volatile(p.add(i), byte);
            }
        }
    }

    pub fn w16(&self, off: usize, v: u16) { self.wr(off, &v.to_le_bytes()); }
    pub fn w32(&self, off: usize, v: u32) { self.wr(off, &v.to_le_bytes()); }
    pub fn w64(&self, off: usize, v: u64) { self.wr(off, &v.to_le_bytes()); }

    pub fn r16(&self, off: usize) -> u16 { let mut b = [0u8; 2]; self.rd(off, &mut b); u16::from_le_bytes(b) }
    pub fn r32(&self, off: usize) -> u32 { let mut b = [0u8; 4]; self.rd(off, &mut b); u32::from_le_bytes(b) }
    pub fn r8(&self, off: usize) -> u8 { let mut b = [0u8; 1]; self.rd(off, &mut b); b[0] }
}

impl Drop for SharedMem {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.size);
            libc::close(self.fd);
        }
    }
}
