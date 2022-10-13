#![forbid(unsafe_op_in_unsafe_fn)]

mod interface_name;
mod socket;

pub use socket::UdpSocket;

mod raw_socket {
    /// This file contains safe wrappers for the socket-related system calls
    /// needed to implement the UdpSocket in socket.rs
    ///
    /// Since the safety of a rust unsafe block depends not only on its
    /// contents, but also the context within which it is called, the code
    /// here is split up in submodules that are individually as small as
    /// possible while still having each a fully safe API interface. This
    /// should reduce the amount of context which needs to be considered
    /// when reasoning about safety, significantly simplifying the checking
    /// of this code.
    ///
    /// All unsafe blocks are preceded with a comment explaining why that
    /// specific unsafe code should be safe within the context in which it
    /// is used.
    pub(crate) use exceptional_condition_fd::exceptional_condition_fd;
    pub(crate) use recv_message::{
        control_message_space, receive_message, ControlMessage, MessageQueue,
    };
    pub(crate) use set_timestamping_options::set_timestamping_options;
    pub(crate) use timestamping_config::TimestampingConfig;

    /// Turn a C failure (-1 is returned) into a rust Result
    pub(crate) fn cerr(t: libc::c_int) -> std::io::Result<libc::c_int> {
        match t {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(t),
        }
    }

    mod set_timestamping_options {
        use std::os::unix::prelude::AsRawFd;

        use super::{cerr, TimestampingConfig};

        pub(crate) fn set_timestamping_options(
            udp_socket: &std::net::UdpSocket,
            timestamping: TimestampingConfig,
        ) -> std::io::Result<()> {
            let fd = udp_socket.as_raw_fd();

            let mut options = 0;

            if timestamping.rx_software || timestamping.tx_software {
                // enable software timestamping
                options |= libc::SOF_TIMESTAMPING_SOFTWARE
            }

            if timestamping.rx_software {
                // we want receive timestamps
                options |= libc::SOF_TIMESTAMPING_RX_SOFTWARE
            }

            if timestamping.tx_software {
                // - we want send timestamps
                // - return just the timestamp, don't send the full message along
                // - tag the timestamp with an ID
                options |= libc::SOF_TIMESTAMPING_TX_SOFTWARE
                    | libc::SOF_TIMESTAMPING_OPT_TSONLY
                    | libc::SOF_TIMESTAMPING_OPT_ID;
            }

            // for documentation on SO_TIMESTAMPING see
            // https://www.kernel.org/doc/Documentation/networking/timestamping.txt
            // Safety:
            // we have a reference to the socket, so fd is a valid file descriptor for the duration of the call
            // SOL_SOCKET + SO_TIMESTAMPING expect a *u32 as value, which &options is. Furthermore, we own options
            // hence the pointer is valid for the duration of the call.
            // option_len is set to the size of u32, which is the size for which the value pointer is valid.
            unsafe {
                cerr(libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TIMESTAMPING,
                    &options as *const _ as *const libc::c_void,
                    std::mem::size_of::<u32>() as libc::socklen_t,
                ))?
            };

            Ok(())
        }
    }

    mod recv_message {
        use std::{
            io::IoSliceMut, marker::PhantomData, net::SocketAddr, os::unix::prelude::AsRawFd,
        };

        use tracing::warn;

        use crate::interface_name::sockaddr_storage_to_socket_addr;

        use super::cerr;

        pub(crate) enum MessageQueue {
            Normal,
            Error,
        }

        pub(crate) fn receive_message<'a>(
            socket: &std::net::UdpSocket,
            packet_buf: &mut [u8],
            control_buf: &'a mut [u8],
            queue: MessageQueue,
        ) -> std::io::Result<(
            libc::c_int,
            impl Iterator<Item = ControlMessage> + 'a,
            Option<SocketAddr>,
        )> {
            let mut buf_slice = IoSliceMut::new(packet_buf);
            let mut addr = zeroed_sockaddr_storage();

            let mut mhdr = libc::msghdr {
                msg_control: control_buf.as_mut_ptr().cast::<libc::c_void>(),
                msg_controllen: control_buf.len(),
                msg_iov: (&mut buf_slice as *mut IoSliceMut).cast::<libc::iovec>(),
                msg_iovlen: 1,
                msg_flags: 0,
                msg_name: (&mut addr as *mut libc::sockaddr_storage).cast::<libc::c_void>(),
                msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as u32,
            };

            let receive_flags = match queue {
                MessageQueue::Normal => 0,
                MessageQueue::Error => libc::MSG_ERRQUEUE,
            };

            // Safety:
            // We have a mutable reference to the control buffer for the duration of the
            // call, and controllen is also set to it's length.
            // IoSliceMut is ABI compatible with iovec, and we only have 1 which matches iovlen
            // msg_name is initialized to point to an owned sockaddr_storage and
            // msg_namelen is the size of sockaddr_storage
            // If one of the buffers is too small, recvmsg cuts of data at appropriate boundary
            let sent_bytes = loop {
                match cerr(
                    unsafe { libc::recvmsg(socket.as_raw_fd(), &mut mhdr, receive_flags) } as _,
                ) {
                    Err(e) if std::io::ErrorKind::Interrupted == e.kind() => {
                        // retry when the recv was interrupted
                        continue;
                    }

                    other => break other,
                }
            }?;

            if mhdr.msg_flags & libc::MSG_TRUNC > 0 {
                warn!(
                    max_len = packet_buf.len(),
                    "truncated packet because it was larger than expected",
                );
            }

            if mhdr.msg_flags & libc::MSG_CTRUNC > 0 {
                warn!("truncated control messages");
            }

            // Clear out the fields for which we are giving up the reference
            mhdr.msg_iov = std::ptr::null_mut();
            mhdr.msg_iovlen = 0;
            mhdr.msg_name = std::ptr::null_mut();
            mhdr.msg_namelen = 0;

            // Safety:
            // recvmsg ensures that the control buffer contains
            // a set of valid control messages and that controllen is
            // the length these take up in the buffer.
            Ok((
                sent_bytes,
                unsafe { ControlMessageIterator::new(mhdr) },
                sockaddr_storage_to_socket_addr(&addr),
            ))
        }

        struct ControlMessageIterator<'a> {
            mhdr: libc::msghdr,
            current_msg: *const libc::cmsghdr,
            phantom: PhantomData<&'a [u8]>,
        }

        impl<'a> ControlMessageIterator<'a> {
            // Safety assumptions:
            // mhdr has a control and controllen field
            // that together describe a memory region
            // with lifetime 'a containing valid control
            // messages
            unsafe fn new(mhdr: libc::msghdr) -> Self {
                // Safety:
                // mhdr's control and controllen fields are valid and point
                // to control messages.
                let current_msg = unsafe { libc::CMSG_FIRSTHDR(&mhdr) };
                Self {
                    mhdr,
                    current_msg,
                    phantom: PhantomData,
                }
            }
        }

        pub(crate) enum ControlMessage {
            Timestamping(libc::timespec),
            ReceiveError(libc::sock_extended_err),
            Other(libc::cmsghdr),
        }

        impl<'a> Iterator for ControlMessageIterator<'a> {
            type Item = ControlMessage;

            fn next(&mut self) -> Option<Self::Item> {
                // Safety:
                // CMSG_FIRSTHDR and CMSG_NXTHDR only return valid pointers or NULL when given valid input
                let current_msg = unsafe { self.current_msg.as_ref() };
                if let Some(current_msg) = current_msg {
                    // Safety:
                    // New ensure mhdr is valid
                    // CMSG_FIRSTHDR and CMSG_NXTHDR only return valid pointers or NULL when given valid input
                    self.current_msg = unsafe { libc::CMSG_NXTHDR(&self.mhdr, self.current_msg) };

                    match (current_msg.cmsg_level, current_msg.cmsg_type) {
                        (libc::SOL_SOCKET, libc::SO_TIMESTAMPING) => {
                            // Safety:
                            // New ensures we have valid control messages
                            // SO_TIMESTAMPING always has a timespec in the data
                            let cmsg_data =
                                unsafe { libc::CMSG_DATA(current_msg) } as *const libc::timespec;
                            let timespec = unsafe { std::ptr::read_unaligned(cmsg_data) };
                            Some(ControlMessage::Timestamping(timespec))
                        }

                        (libc::SOL_IP, libc::IP_RECVERR) | (libc::SOL_IPV6, libc::IPV6_RECVERR) => {
                            // this is part of how timestamps are reported.
                            // Safety:
                            // New ensures we have valid control messages
                            // IP*_RECVERR always has a sock_extended_err in the data
                            let error = unsafe {
                                let ptr =
                                    libc::CMSG_DATA(current_msg) as *const libc::sock_extended_err;
                                std::ptr::read_unaligned(ptr)
                            };

                            Some(ControlMessage::ReceiveError(error))
                        }
                        _ => Some(ControlMessage::Other(*current_msg)),
                    }
                } else {
                    None
                }
            }
        }

        /// The space used to store a control message that contains a value of type T
        pub(crate) const fn control_message_space<T>() -> usize {
            // Safety: CMSG_SPACE is safe to call
            (unsafe { libc::CMSG_SPACE((std::mem::size_of::<T>()) as _) }) as usize
        }

        fn zeroed_sockaddr_storage() -> libc::sockaddr_storage {
            // a zeroed-out sockaddr storage is semantically valid, because a ss_family with value 0 is
            // libc::AF_UNSPEC. Hence the rest of the data does not come with any constraints
            // Safety:
            // the MaybeUninit is zeroed before assumed to be initialized
            unsafe { std::mem::MaybeUninit::zeroed().assume_init() }
        }
    }

    mod timestamping_config {
        use std::os::unix::prelude::AsRawFd;

        use super::cerr;
        use crate::interface_name;

        #[derive(Debug, Clone, Copy, Default)]
        pub(crate) struct TimestampingConfig {
            pub(crate) rx_software: bool,
            pub(crate) tx_software: bool,
        }

        #[repr(C)]
        #[allow(non_camel_case_types)]
        #[derive(Default)]
        struct ethtool_ts_info {
            cmd: u32,
            so_timestamping: u32,
            phc_index: u32,
            tx_types: u32,
            tx_reserved: [u32; 3],
            rx_filters: u32,
            rx_reserved: [u32; 3],
        }

        /// source: https://github.com/torvalds/linux/blob/master/include/uapi/linux/if.h#L241
        #[repr(C)]
        union ifr_ifru {
            ifr_addr: libc::sockaddr,
            ifr_dstaddr: libc::sockaddr,
            ifr_broadaddr: libc::sockaddr,
            ifr_netmask: libc::sockaddr,
            ifr_hwaddr: libc::sockaddr,
            ifr_flags: libc::c_short,
            ifr_ifindex: libc::c_int,
            ifr_metric: libc::c_int,
            ifr_mtu: libc::c_int,
            ifr_map: ifmap,
            ifr_slave: [libc::c_char; libc::IFNAMSIZ],
            ifr_newname: [libc::c_char; libc::IFNAMSIZ],
            ifr_data: *mut libc::c_char,
        }

        /// source: https://github.com/torvalds/linux/blob/master/include/uapi/linux/if.h#L196
        #[repr(C)]
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        struct ifmap {
            mem_start: libc::c_ulong,
            mem_end: libc::c_ulong,
            base_addr: libc::c_ushort,
            irq: libc::c_uchar,
            dma: libc::c_uchar,
            port: libc::c_uchar,
        }

        /// source: https://github.com/torvalds/linux/blob/master/include/uapi/linux/if.h#L234
        #[repr(C)]
        #[allow(non_camel_case_types)]
        struct ifreq {
            ifr_name: [u8; libc::IFNAMSIZ],
            ifr_ifru: ifr_ifru,
        }

        impl TimestampingConfig {
            /// Enable all timestamping options that are supported by this crate and the hardware/software
            /// of the device we're running on
            #[allow(dead_code)]
            pub(crate) fn all_supported(udp_socket: &std::net::UdpSocket) -> std::io::Result<Self> {
                // Get time stamping and PHC info
                const ETHTOOL_GET_TS_INFO: u32 = 0x00000041;

                let mut tsi: ethtool_ts_info = ethtool_ts_info {
                    cmd: ETHTOOL_GET_TS_INFO,
                    ..Default::default()
                };

                let fd = udp_socket.as_raw_fd();

                if let Some(ifr_name) = interface_name::interface_name(udp_socket.local_addr()?)? {
                    let ifr: ifreq = ifreq {
                        ifr_name,
                        ifr_ifru: ifr_ifru {
                            ifr_data: (&mut tsi as *mut _) as *mut libc::c_char,
                        },
                    };

                    const SIOCETHTOOL: u64 = 0x8946;
                    cerr(unsafe { libc::ioctl(fd, SIOCETHTOOL as libc::c_ulong, &ifr) }).unwrap();

                    let support = Self {
                        rx_software: tsi.so_timestamping & libc::SOF_TIMESTAMPING_RX_SOFTWARE != 0,
                        tx_software: tsi.so_timestamping & libc::SOF_TIMESTAMPING_TX_SOFTWARE != 0,
                    };

                    // per the documentation of `SOF_TIMESTAMPING_RX_SOFTWARE`:
                    //
                    // > Request rx timestamps when data enters the kernel. These timestamps are generated
                    // > just after a device driver hands a packet to the kernel receive stack.
                    //
                    // the linux kernal should always support receive software timestamping
                    assert!(support.rx_software);

                    Ok(support)
                } else {
                    Ok(Self::default())
                }
            }
        }
    }

    mod exceptional_condition_fd {
        use std::os::unix::prelude::{AsRawFd, RawFd};

        use tokio::io::unix::AsyncFd;

        use super::cerr;

        // Tokio does not natively support polling for readiness of queues
        // other than the normal read queue (see also https://github.com/tokio-rs/tokio/issues/4885)
        // this works around that by creating a epoll fd that becomes
        // ready to read when the underlying fd has an event on its error queue.
        pub(crate) fn exceptional_condition_fd(
            socket_of_interest: &std::net::UdpSocket,
        ) -> std::io::Result<AsyncFd<RawFd>> {
            // Safety:
            // epoll_create1 is safe to call without flags
            let fd = cerr(unsafe { libc::epoll_create1(0) })?;

            let mut event = libc::epoll_event {
                events: libc::EPOLLPRI as u32,
                u64: 0u64,
            };

            // Safety:
            // fd is a valid epoll fd from epoll_create1 in combination with the cerr check
            // since we have a reference to the socket_of_interest, its raw fd
            // is valid for the duration of this call, which is all that is
            // required for epoll (closing the fd later is safe!)
            // &mut event is a pointer to a memory region which we own for the duration
            // of the call, and thus ok to use.
            cerr(unsafe {
                libc::epoll_ctl(
                    fd,
                    libc::EPOLL_CTL_ADD,
                    socket_of_interest.as_raw_fd(),
                    &mut event,
                )
            })?;

            AsyncFd::new(fd)
        }
    }
}
