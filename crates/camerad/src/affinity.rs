//! Pinning a camera's threads to CPUs. Linux gives a new thread its creator's CPU set, so the
//! daemon starts each camera's service from a thread pinned to that camera's CPUs: every
//! thread the service makes later (its accept loop, each client's connection, the capture, ISP
//! and 3A workers they start) is a descendant and stays on them.

/// Pins the calling thread to `cpus`. An empty list, or CPUs this machine lacks, is an error.
pub fn pin_current_thread(cpus: &[usize]) -> std::io::Result<()> {
    // SAFETY: an all-zero cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    let max = 8 * core::mem::size_of::<libc::cpu_set_t>();
    let mut any = false;
    for &cpu in cpus {
        if cpu >= max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("CPU {cpu} is beyond the {max} a CPU set holds"),
            ));
        }
        // SAFETY: `cpu` is below the set's capacity, checked above.
        unsafe { libc::CPU_SET(cpu, &mut set) };
        any = true;
    }
    if !any {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "an empty CPU list",
        ));
    }
    // SAFETY: pid 0 is the calling thread; `set` is a valid set of its size.
    let rc = unsafe { libc::sched_setaffinity(0, core::mem::size_of_val(&set), &set) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The CPUs the calling thread may run on.
pub fn current_thread_cpus() -> std::io::Result<Vec<usize>> {
    // SAFETY: an all-zero cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    // SAFETY: pid 0 is the calling thread; `set` is writable and of the size given.
    let rc = unsafe { libc::sched_getaffinity(0, core::mem::size_of_val(&set), &mut set) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let max = 8 * core::mem::size_of::<libc::cpu_set_t>();
    // SAFETY: every index is below the set's capacity.
    Ok((0..max)
        .filter(|&c| unsafe { libc::CPU_ISSET(c, &set) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_started_from_a_pinned_thread_keeps_its_cpus() {
        let first = current_thread_cpus().unwrap()[0];
        let inherited = std::thread::spawn(move || {
            pin_current_thread(&[first]).unwrap();
            std::thread::spawn(current_thread_cpus)
                .join()
                .unwrap()
                .unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(inherited, [first]);
    }

    #[test]
    fn an_empty_list_is_refused() {
        assert!(pin_current_thread(&[]).is_err());
    }
}
