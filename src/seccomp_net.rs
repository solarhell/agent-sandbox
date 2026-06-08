#[cfg(target_os = "linux")]
mod imp {
    use anyhow::bail;
    use libc::{
        BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, EACCES, PR_SET_NO_NEW_PRIVS,
        PR_SET_SECCOMP, SECCOMP_MODE_FILTER, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO,
        SECCOMP_RET_KILL_PROCESS, c_ushort, prctl, sock_filter, sock_fprog,
    };

    const SECCOMP_DATA_NR_OFFSET: u32 = 0;
    const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;

    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    pub fn install_network_deny_filter() -> anyhow::Result<()> {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let syscalls = network_syscalls();
            let mut filter = Vec::with_capacity(syscalls.len() * 2 + 5);
            filter.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET));
            filter.push(jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH, 1, 0));
            filter.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));
            filter.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));
            for &syscall in syscalls {
                filter.push(jump(BPF_JMP | BPF_JEQ | BPF_K, syscall, 0, 1));
                filter.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EACCES as u32)));
            }
            filter.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

            let mut program = sock_fprog {
                len: filter
                    .len()
                    .try_into()
                    .expect("seccomp filter length fits c_ushort"),
                filter: filter.as_mut_ptr(),
            };

            // Landlock normally sets no_new_privs too, but seccomp requires it.
            prctl_check(
                unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
                "PR_SET_NO_NEW_PRIVS",
            )?;
            prctl_check(
                unsafe { prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &mut program) },
                "PR_SET_SECCOMP",
            )?;
            Ok(())
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            bail!("seccomp network deny filter is not implemented for this CPU architecture")
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn network_syscalls() -> &'static [u32] {
        &[
            libc::SYS_socket as u32,
            libc::SYS_socketpair as u32,
            libc::SYS_connect as u32,
            libc::SYS_accept as u32,
            libc::SYS_accept4 as u32,
            libc::SYS_bind as u32,
            libc::SYS_listen as u32,
            libc::SYS_sendto as u32,
            libc::SYS_sendmsg as u32,
            libc::SYS_sendmmsg as u32,
            libc::SYS_recvfrom as u32,
            libc::SYS_recvmsg as u32,
            libc::SYS_recvmmsg as u32,
        ]
    }

    #[cfg(target_arch = "aarch64")]
    fn network_syscalls() -> &'static [u32] {
        &[
            libc::SYS_socket as u32,
            libc::SYS_socketpair as u32,
            libc::SYS_connect as u32,
            libc::SYS_accept as u32,
            libc::SYS_accept4 as u32,
            libc::SYS_bind as u32,
            libc::SYS_listen as u32,
            libc::SYS_sendto as u32,
            libc::SYS_sendmsg as u32,
            libc::SYS_sendmmsg as u32,
            libc::SYS_recvfrom as u32,
            libc::SYS_recvmsg as u32,
            libc::SYS_recvmmsg as u32,
        ]
    }

    fn stmt(code: u32, k: u32) -> sock_filter {
        sock_filter {
            code: bpf_code(code),
            jt: 0,
            jf: 0,
            k,
        }
    }

    fn jump(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: bpf_code(code),
            jt,
            jf,
            k,
        }
    }

    fn bpf_code(code: u32) -> c_ushort {
        code.try_into().expect("BPF instruction code fits u16")
    }

    fn prctl_check(result: i32, operation: &str) -> anyhow::Result<()> {
        if result == -1 {
            bail!("{operation} failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn install_network_deny_filter() -> anyhow::Result<()> {
        anyhow::bail!("seccomp network deny filter requires Linux");
    }
}

pub use imp::install_network_deny_filter;
