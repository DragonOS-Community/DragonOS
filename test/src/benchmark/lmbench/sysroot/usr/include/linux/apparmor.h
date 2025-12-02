/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef _LINUX_APPARMOR_H
#define _LINUX_APPARMOR_H

#include <linux/types.h>

#define APPARMOR_MODESET_AUDIT 1
#define APPARMOR_MODESET_ALLOWED 2
#define APPARMOR_MODESET_ENFORCE 4
#define APPARMOR_MODESET_HINT 8
#define APPARMOR_MODESET_STATUS 16
#define APPARMOR_MODESET_ERROR 32
#define APPARMOR_MODESET_KILL 64
#define APPARMOR_MODESET_USER 128

#define APPARMOR_FLAG_NOCACHE 1

enum apparmor_notif_type {
	APPARMOR_NOTIF_RESP_PERM,
	APPARMOR_NOTIF_CANCEL,
	APPARMOR_NOTIF_INTERUPT,
	APPARMOR_NOTIF_ALIVE,
	APPARMOR_NOTIF_OP,
	APPARMOR_NOTIF_RESP_NAME,
};

#define APPARMOR_NOTIFY_VERSION 3

/* base notification struct embedded as head of notifications to userspace */
struct apparmor_notif_common {
	__u16 len;			/* actual len data */
	__u16 version;			/* interface version */
} __attribute__((packed));

struct apparmor_notif_filter {
	struct apparmor_notif_common base;
	__u32 modeset;			/* which notification mode */
	__u32 ns;			/* offset into data */
	__u32 filter;			/* offset into data */

	__u8 data[];
} __attribute__((packed));

// flags
#define URESPONSE_NO_CACHE 1
#define URESPONSE_LOOKUP 2
#define URESPONSE_PROFILE 4
#define URESPONSE_TAILGLOB 8

struct apparmor_notif {
	struct apparmor_notif_common base;
	__u16 ntype;			/* notify type */
	__u8 signalled;
	__u8 flags;
	__u64 id;			/* unique id, not gloablly unique*/
	__s32 error;			/* error if unchanged */
} __attribute__((packed));


struct apparmor_notif_update {
	struct apparmor_notif base;
	__u16 ttl;			/* max keep alives left */
} __attribute__((packed));

/* userspace response to notification that expects a response */
struct apparmor_notif_resp_perm {
	struct apparmor_notif base;
	__s32 error;			/* error if unchanged */
	__u32 allow;
	__u32 deny;
} __attribute__((packed));

struct apparmor_notif_resp_name {
	union {
		struct apparmor_notif base;
		struct apparmor_notif_resp_perm perm;
	};
	__u32 name;
	__u8 data[];
} __attribute__((packed));

union apparmor_notif_resp {
	struct apparmor_notif base;
	struct apparmor_notif_resp_perm perm;
	struct apparmor_notif_resp_name name;
} __attribute__((packed));

struct apparmor_notif_op {
	struct apparmor_notif base;
	__u32 allow;
	__u32 deny;
	pid_t pid;			/* pid of task causing notification */
	__u32 label;			/* offset into data */
	__u16 class;
	__u16 op;
} __attribute__((packed));

struct apparmor_notif_file {
	struct apparmor_notif_op base;
	uid_t subj_uid, obj_uid;
	__u32 name;			/* offset into data */

	__u8 data[];
} __attribute__((packed));

union apparmor_notif_all {
	struct apparmor_notif_common common;
	struct apparmor_notif_filter filter;
	struct apparmor_notif base;
	struct apparmor_notif_op op;
	struct apparmor_notif_file file;
	union apparmor_notif_resp respnse;
};

#define APPARMOR_IOC_MAGIC             0xF8

/* Flags for apparmor notification fd ioctl. */

#define APPARMOR_NOTIF_SET_FILTER      _IOW(APPARMOR_IOC_MAGIC, 0,     \
						struct apparmor_notif_filter *)
#define APPARMOR_NOTIF_GET_FILTER      _IOR(APPARMOR_IOC_MAGIC, 1,     \
						struct apparmor_notif_filter *)
#define APPARMOR_NOTIF_IS_ID_VALID     _IOR(APPARMOR_IOC_MAGIC, 3,     \
						__u64)
/* RECV/SEND from userspace pov */
#define APPARMOR_NOTIF_RECV            _IOWR(APPARMOR_IOC_MAGIC, 4,     \
						struct apparmor_notif *)
#define APPARMOR_NOTIF_SEND            _IOWR(APPARMOR_IOC_MAGIC, 5,    \
						union apparmor_notif_resp *)

#endif /* _LINUX_APPARMOR_H */
