#!/bin/busybox sh

# Shared, non-executable configuration transaction helpers for dragon-network.
# Configuration and lease files are parsed as data; they are never sourced.

DN_BUSYBOX=/bin/busybox
DN_RUN=/run/dragon-network
DN_INTERFACES_RUN="$DN_RUN/interfaces"
DN_RESOLV_CONF=/etc/resolv.conf
DN_NETWORK_CONFIG_DIR=/etc/dragonos/network
DN_CONFIG_DIR="$DN_NETWORK_CONFIG_DIR/interfaces"
DN_DEFAULT_CONFIG="$DN_NETWORK_CONFIG_DIR/default.conf"
DN_UDHCPC_SCRIPT=/usr/lib/dragon-network/udhcpc.script

dn_log() {
    echo "dragon-network[$1]: $2" >&2
}

dn_valid_ifname() {
    local name first rest
    name=$1
    [ -n "$name" ] && [ "${#name}" -le 15 ] || return 1
    [ "$name" != "." ] && [ "$name" != ".." ] || return 1
    first=${name%"${name#?}"}
    rest=${name#?}
    case "$first" in
        [A-Za-z0-9_]) ;;
        *) return 1 ;;
    esac
    case "$rest" in
        *[!A-Za-z0-9_.-]*) return 1 ;;
    esac
    return 0
}

dn_valid_uint() {
    case "$1" in
        "" | *[!0-9]*) return 1 ;;
    esac
    return 0
}

dn_valid_ipv4() {
    local value old_ifs octet
    value=$1
    case "$value" in
        "" | *[!0-9.]* | .* | *. | *..*) return 1 ;;
    esac

    old_ifs=$IFS
    IFS=.
    set -- $value
    IFS=$old_ifs
    [ "$#" -eq 4 ] || return 1
    for octet in "$@"; do
        case "$octet" in
            "" | *[!0-9]*) return 1 ;;
            0 | [1-9] | [1-9][0-9] | [1-9][0-9][0-9]) ;;
            *) return 1 ;;
        esac
        [ "$octet" -le 255 ] || return 1
    done
    return 0
}

dn_valid_host_ipv4() {
    local address first
    address=$1
    dn_valid_ipv4 "$address" || return 1
    first=${address%%.*}
    # Match modern Linux semantics: most of 0/8 and 240/4 are ordinary
    # unicast addresses, while ANY, BROADCAST, and 224/4 are not host leases.
    [ "$address" != 0.0.0.0 ] && [ "$address" != 255.255.255.255 ] &&
        { [ "$first" -lt 224 ] || [ "$first" -gt 239 ]; }
}

dn_valid_prefix() {
    dn_valid_uint "$1" && [ "$1" -le 32 ]
}

dn_valid_cidr() {
    local cidr address prefix
    cidr=$1
    case "$cidr" in
        */*) ;;
        *) return 1 ;;
    esac
    address=${cidr%/*}
    prefix=${cidr#*/}
    [ "$address/$prefix" = "$cidr" ] || return 1
    dn_valid_host_ipv4 "$address" && dn_valid_prefix "$prefix"
}

dn_prefix_from_netmask() {
    case "$1" in
        0.0.0.0) DN_PREFIX=0 ;;
        128.0.0.0) DN_PREFIX=1 ;;
        192.0.0.0) DN_PREFIX=2 ;;
        224.0.0.0) DN_PREFIX=3 ;;
        240.0.0.0) DN_PREFIX=4 ;;
        248.0.0.0) DN_PREFIX=5 ;;
        252.0.0.0) DN_PREFIX=6 ;;
        254.0.0.0) DN_PREFIX=7 ;;
        255.0.0.0) DN_PREFIX=8 ;;
        255.128.0.0) DN_PREFIX=9 ;;
        255.192.0.0) DN_PREFIX=10 ;;
        255.224.0.0) DN_PREFIX=11 ;;
        255.240.0.0) DN_PREFIX=12 ;;
        255.248.0.0) DN_PREFIX=13 ;;
        255.252.0.0) DN_PREFIX=14 ;;
        255.254.0.0) DN_PREFIX=15 ;;
        255.255.0.0) DN_PREFIX=16 ;;
        255.255.128.0) DN_PREFIX=17 ;;
        255.255.192.0) DN_PREFIX=18 ;;
        255.255.224.0) DN_PREFIX=19 ;;
        255.255.240.0) DN_PREFIX=20 ;;
        255.255.248.0) DN_PREFIX=21 ;;
        255.255.252.0) DN_PREFIX=22 ;;
        255.255.254.0) DN_PREFIX=23 ;;
        255.255.255.0) DN_PREFIX=24 ;;
        255.255.255.128) DN_PREFIX=25 ;;
        255.255.255.192) DN_PREFIX=26 ;;
        255.255.255.224) DN_PREFIX=27 ;;
        255.255.255.240) DN_PREFIX=28 ;;
        255.255.255.248) DN_PREFIX=29 ;;
        255.255.255.252) DN_PREFIX=30 ;;
        255.255.255.254) DN_PREFIX=31 ;;
        255.255.255.255) DN_PREFIX=32 ;;
        *) return 1 ;;
    esac
    return 0
}

dn_valid_domain() {
    local domain old_ifs label
    domain=$1
    [ -n "$domain" ] && [ "${#domain}" -le 253 ] || return 1
    case "$domain" in
        .* | *. | *..* | *[!A-Za-z0-9.-]*) return 1 ;;
    esac

    old_ifs=$IFS
    IFS=.
    set -- $domain
    IFS=$old_ifs
    for label in "$@"; do
        [ -n "$label" ] && [ "${#label}" -le 63 ] || return 1
        case "$label" in
            -* | *-) return 1 ;;
        esac
    done
    return 0
}

dn_valid_ipv4_list() {
    local value
    for value in $1; do
        dn_valid_host_ipv4 "$value" || return 1
    done
    return 0
}

dn_valid_domain_list() {
    local value total
    total=0
    for value in $1; do
        dn_valid_domain "$value" || return 1
        total=$((total + ${#value} + 1))
        [ "$total" -le 256 ] || return 1
    done
    return 0
}

dn_stat_is_root_mode() {
    local path expected uid mode kind
    path=$1
    expected=$2
    [ ! -L "$path" ] || return 1
    uid=$($DN_BUSYBOX stat -c %u "$path" 2>/dev/null) || return 1
    mode=$($DN_BUSYBOX stat -c %a "$path" 2>/dev/null) || return 1
    kind=$($DN_BUSYBOX stat -c %F "$path" 2>/dev/null) || return 1
    [ "$uid" = 0 ] && [ "$mode" = "$expected" ] && [ "$kind" = directory ]
}

dn_is_fat_path() {
    # Linux UAPI MSDOS_SUPER_MAGIC. DragonOS FAT currently synthesizes 0777
    # because the on-disk format has no Unix mode bits.
    [ "$($DN_BUSYBOX stat -f -c %t "$1" 2>/dev/null)" = 4d44 ]
}

dn_stat_is_root_config_dir() {
    local path uid mode kind
    path=$1
    [ ! -L "$path" ] || return 1
    uid=$($DN_BUSYBOX stat -c %u "$path" 2>/dev/null) || return 1
    mode=$($DN_BUSYBOX stat -c %a "$path" 2>/dev/null) || return 1
    kind=$($DN_BUSYBOX stat -c %F "$path" 2>/dev/null) || return 1
    [ "$uid" = 0 ] && [ "$kind" = directory ] || return 1
    case "$mode" in
        [0-7][0-7][0-7]) ;;
        *) return 1 ;;
    esac
    [ "$((0$mode & 022))" -eq 0 ] || { [ "$mode" = 777 ] && dn_is_fat_path "$path"; }
}

dn_require_config_dir_chain() {
    local path
    for path in "$@"; do
        dn_stat_is_root_config_dir "$path" || return 1
    done
}

dn_ensure_root_dir() {
    local path mode
    path=$1
    mode=$2
    if [ ! -e "$path" ]; then
        $DN_BUSYBOX mkdir -m "$mode" "$path" 2>/dev/null || :
    fi
    dn_stat_is_root_mode "$path" "$mode"
}

dn_require_config_file() {
    local path uid mode kind
    path=$1
    [ -f "$path" ] && [ ! -L "$path" ] || return 1
    uid=$($DN_BUSYBOX stat -c %u "$path" 2>/dev/null) || return 1
    mode=$($DN_BUSYBOX stat -c %a "$path" 2>/dev/null) || return 1
    kind=$($DN_BUSYBOX stat -c %F "$path" 2>/dev/null) || return 1
    [ "$uid" = 0 ] && [ "$kind" = "regular file" ] || return 1
    case "$mode" in
        [0-7][0-7][0-7]) ;;
        *) return 1 ;;
    esac
    # Execute bits are harmless for parsed data.  On filesystems with Unix modes,
    # only root may write it.  DragonOS FAT synthesizes 0777, so that exception is
    # limited to a verified FAT superblock; the strict parser and root-only
    # committed runtime generation remain the policy boundary.
    [ "$((0$mode & 022))" -eq 0 ] || { [ "$mode" = 777 ] && dn_is_fat_path "$path"; }
}

dn_reset_config_vars() {
    DN_CONFIG_MODE=
    DN_CONFIG_ADDRESS=
    DN_CONFIG_GATEWAY=
    DN_CONFIG_DNS=
    DN_CONFIG_SEARCH=
}

dn_load_config_file() {
    local iface file line key value seen_mode seen_address seen_gateway
    iface=$1
    file=$2
    dn_require_config_file "$file" || {
        dn_log "$iface" "configuration must be a root-owned regular file not writable by group or other: $file"
        return 1
    }

    dn_reset_config_vars
    seen_mode=0
    seen_address=0
    seen_gateway=0
    while IFS= read -r line || [ -n "$line" ]; do
        line=${line%%#*}
        set -- $line
        [ "$#" -gt 0 ] || continue
        [ "$#" -eq 2 ] || {
            dn_log "$iface" "invalid configuration record"
            return 1
        }
        key=$1
        value=$2
        case "$key" in
            mode)
                [ "$seen_mode" -eq 0 ] || return 1
                case "$value" in
                    dhcp | static) DN_CONFIG_MODE=$value ;;
                    external | unmanaged) DN_CONFIG_MODE=unmanaged ;;
                    *) return 1 ;;
                esac
                seen_mode=1
                ;;
            address)
                [ "$seen_address" -eq 0 ] && dn_valid_cidr "$value" || return 1
                DN_CONFIG_ADDRESS=$value
                seen_address=1
                ;;
            gateway)
                [ "$seen_gateway" -eq 0 ] && dn_valid_host_ipv4 "$value" || return 1
                DN_CONFIG_GATEWAY=$value
                seen_gateway=1
                ;;
            dns)
                dn_valid_host_ipv4 "$value" || return 1
                DN_CONFIG_DNS="${DN_CONFIG_DNS}${DN_CONFIG_DNS:+ }$value"
                ;;
            search)
                dn_valid_domain "$value" || return 1
                DN_CONFIG_SEARCH="${DN_CONFIG_SEARCH}${DN_CONFIG_SEARCH:+ }$value"
                dn_valid_domain_list "$DN_CONFIG_SEARCH" || return 1
                ;;
            *) return 1 ;;
        esac
    done < "$file"

    [ "$seen_mode" -eq 1 ] || return 1
    case "$DN_CONFIG_MODE" in
        dhcp | unmanaged)
            [ "$seen_address" -eq 0 ] && [ "$seen_gateway" -eq 0 ] &&
                [ -z "$DN_CONFIG_DNS" ] && [ -z "$DN_CONFIG_SEARCH" ] || return 1
            ;;
        static)
            [ "$seen_address" -eq 1 ] || return 1
            ;;
    esac
    return 0
}

dn_load_config() {
    local iface
    iface=$1
    dn_require_config_dir_chain / /etc /etc/dragonos "$DN_NETWORK_CONFIG_DIR" \
        "$DN_CONFIG_DIR" || {
        dn_log "$iface" "configuration directory chain is not trusted"
        return 1
    }
    dn_load_config_file "$iface" "$DN_CONFIG_DIR/$iface.conf"
}

dn_load_default_config() {
    local iface
    iface=$1
    dn_require_config_dir_chain / /etc /etc/dragonos "$DN_NETWORK_CONFIG_DIR" || {
        dn_log "$iface" "default configuration directory chain is not trusted"
        return 1
    }
    dn_load_config_file "$iface" "$DN_DEFAULT_CONFIG"
}

dn_init_runtime() {
    local fs_type
    [ -d /run ] && [ ! -L /run ] || {
        dn_log runtime "/run is not a safe directory"
        return 1
    }
    $DN_BUSYBOX mountpoint -q /run || {
        dn_log runtime "/run is not a mount point"
        return 1
    }
    fs_type=$($DN_BUSYBOX stat -f -c %T /run 2>/dev/null) || return 1
    [ "$fs_type" = tmpfs ] || {
        dn_log runtime "/run must be tmpfs, found $fs_type"
        return 1
    }

    dn_ensure_root_dir "$DN_RUN" 755 || {
        dn_log runtime "$DN_RUN must be a root-owned 0755 directory"
        return 1
    }

    dn_ensure_root_dir "$DN_INTERFACES_RUN" 700 || {
        dn_log runtime "$DN_INTERFACES_RUN must be a root-owned 0700 directory"
        return 1
    }
    return 0
}

dn_init_interface_runtime() {
    local iface dir
    iface=$1
    dn_valid_ifname "$iface" || return 1
    dir="$DN_INTERFACES_RUN/$iface"
    dn_ensure_root_dir "$dir" 700 || {
        dn_log "$iface" "$dir must be a root-owned 0700 directory"
        return 1
    }
    DN_IFACE_RUN=$dir
    DN_LEASE_FILE="$dir/lease"
    DN_PENDING_FILE="$dir/pending"
    DN_PID_FILE="$dir/udhcpc.pid"
    DN_ERROR_FILE="$dir/last-error"
    DN_POLICY_SOURCE_FILE="$dir/policy-source"
    return 0
}

dn_write_policy_source() {
    local source tmp
    source=$1
    case "$source" in default | interface) ;; *) return 1 ;; esac
    tmp="$DN_POLICY_SOURCE_FILE.tmp.$$"
    (umask 077 && printf 'source %s\n' "$source" > "$tmp") || {
        $DN_BUSYBOX rm -f "$tmp"
        return 1
    }
    $DN_BUSYBOX chmod 0600 "$tmp" && $DN_BUSYBOX mv -f "$tmp" "$DN_POLICY_SOURCE_FILE" || {
        $DN_BUSYBOX rm -f "$tmp"
        return 1
    }
}

dn_clear_policy_source() {
    [ ! -e "$DN_POLICY_SOURCE_FILE" ] && [ ! -L "$DN_POLICY_SOURCE_FILE" ] ||
        $DN_BUSYBOX rm -f "$DN_POLICY_SOURCE_FILE"
}

dn_load_policy_source() {
    local key value extra uid mode kind seen
    [ -f "$DN_POLICY_SOURCE_FILE" ] && [ ! -L "$DN_POLICY_SOURCE_FILE" ] || return 1
    uid=$($DN_BUSYBOX stat -c %u "$DN_POLICY_SOURCE_FILE" 2>/dev/null) || return 1
    mode=$($DN_BUSYBOX stat -c %a "$DN_POLICY_SOURCE_FILE" 2>/dev/null) || return 1
    kind=$($DN_BUSYBOX stat -c %F "$DN_POLICY_SOURCE_FILE" 2>/dev/null) || return 1
    [ "$uid" = 0 ] && [ "$mode" = 600 ] && [ "$kind" = "regular file" ] || return 1
    seen=0
    while IFS=' ' read -r key value extra || [ -n "$key$value$extra" ]; do
        [ "$seen" -eq 0 ] && [ "$key" = source ] && [ -n "$value" ] &&
            [ -z "$extra" ] || return 1
        case "$value" in default | interface) DN_POLICY_SOURCE=$value ;; *) return 1 ;; esac
        seen=1
    done < "$DN_POLICY_SOURCE_FILE"
    [ "$seen" -eq 1 ]
}

dn_require_active_dhcp_policy() {
    # The root-only runtime marker is the committed policy generation. Config
    # edits take effect only through a serialized controller start, just like
    # other network managers; lease callbacks must not re-read mutable storage.
    dn_load_policy_source
}

dn_write_error() {
    local iface message tmp
    iface=$1
    message=$2
    tmp="$DN_ERROR_FILE.tmp.$$"
    if (umask 077 && printf '%s\n' "$message" > "$tmp" && $DN_BUSYBOX chmod 0600 "$tmp"); then
        $DN_BUSYBOX mv -f "$tmp" "$DN_ERROR_FILE" || $DN_BUSYBOX rm -f "$tmp"
    else
        $DN_BUSYBOX rm -f "$tmp"
    fi
    dn_log "$iface" "$message"
}

dn_clear_error() {
    $DN_BUSYBOX rm -f "$DN_ERROR_FILE"
}

dn_acquire_state_lock() {
    local lock
    lock="$DN_IFACE_RUN/state.lock"
    [ ! -L "$lock" ] || return 1
    : > "$lock" || return 1
    $DN_BUSYBOX chmod 0600 "$lock" || return 1
    exec 7>"$lock" || return 1
    $DN_BUSYBOX flock -x 7
}

dn_reset_lease_vars() {
    DN_LEASE_PRESENT=0
    DN_LEASE_OWNER=
    DN_LEASE_ADDRESS=
    DN_LEASE_ROUTER=
    DN_LEASE_GATEWAY_ROUTE=
    DN_LEASE_DNS=
    DN_LEASE_SEARCH=
}

dn_load_lease() {
    local file key value extra seen_owner seen_address seen_router seen_gateway
    file=$1
    dn_reset_lease_vars
    [ -e "$file" ] || return 0
    [ -f "$file" ] && [ ! -L "$file" ] || return 1
    seen_owner=0
    seen_address=0
    seen_router=0
    seen_gateway=0
    while IFS=' ' read -r key value extra; do
        [ -n "$key" ] || continue
        [ -n "$value" ] && [ -z "$extra" ] || return 1
        case "$key" in
            owner)
                [ "$seen_owner" -eq 0 ] || return 1
                case "$value" in dhcp | static) ;; *) return 1 ;; esac
                DN_LEASE_OWNER=$value
                seen_owner=1
                ;;
            address)
                [ "$seen_address" -eq 0 ] && dn_valid_cidr "$value" || return 1
                DN_LEASE_ADDRESS=$value
                seen_address=1
                ;;
            router)
                [ "$seen_router" -eq 0 ] && dn_valid_host_ipv4 "$value" || return 1
                DN_LEASE_ROUTER=$value
                seen_router=1
                ;;
            gateway-route)
                [ "$seen_gateway" -eq 0 ] && dn_valid_cidr "$value" || return 1
                [ "${value#*/}" = 32 ] || return 1
                DN_LEASE_GATEWAY_ROUTE=$value
                seen_gateway=1
                ;;
            dns)
                dn_valid_host_ipv4 "$value" || return 1
                DN_LEASE_DNS="${DN_LEASE_DNS}${DN_LEASE_DNS:+ }$value"
                ;;
            search)
                dn_valid_domain "$value" || return 1
                DN_LEASE_SEARCH="${DN_LEASE_SEARCH}${DN_LEASE_SEARCH:+ }$value"
                ;;
            *) return 1 ;;
        esac
    done < "$file"
    [ "$seen_owner" -eq 1 ] && [ "$seen_address" -eq 1 ] || return 1
    [ -z "$DN_LEASE_GATEWAY_ROUTE" ] || [ -n "$DN_LEASE_ROUTER" ] || return 1
    DN_LEASE_PRESENT=1
    return 0
}

dn_write_lease() {
    local owner address router gateway_route dns search tmp value
    owner=$1
    address=$2
    router=$3
    gateway_route=$4
    dns=$5
    search=$6
    tmp="$DN_LEASE_FILE.tmp.$$"
    (
        umask 077
        printf 'owner %s\naddress %s\n' "$owner" "$address"
        [ -z "$router" ] || printf 'router %s\n' "$router"
        [ -z "$gateway_route" ] || printf 'gateway-route %s\n' "$gateway_route"
        for value in $dns; do printf 'dns %s\n' "$value"; done
        for value in $search; do printf 'search %s\n' "$value"; done
    ) > "$tmp" || {
        $DN_BUSYBOX rm -f "$tmp"
        return 1
    }
    $DN_BUSYBOX chmod 0600 "$tmp" && $DN_BUSYBOX mv -f "$tmp" "$DN_LEASE_FILE" || {
        $DN_BUSYBOX rm -f "$tmp"
        return 1
    }
}

dn_write_pending() {
    local old_address old_router old_gateway new_address new_router new_gateway tmp
    old_address=$1
    old_router=$2
    old_gateway=$3
    new_address=$4
    new_router=$5
    new_gateway=$6
    tmp="$DN_PENDING_FILE.tmp.$$"
    (
        umask 077
        [ -z "$old_address" ] || printf 'owned-address %s\n' "$old_address"
        [ -z "$new_address" ] || [ "$new_address" = "$old_address" ] || printf 'owned-address %s\n' "$new_address"
        [ -z "$old_router" ] || printf 'owned-router %s\n' "$old_router"
        [ -z "$new_router" ] || [ "$new_router" = "$old_router" ] || printf 'owned-router %s\n' "$new_router"
        [ -z "$old_gateway" ] || printf 'owned-gateway-route %s\n' "$old_gateway"
        [ -z "$new_gateway" ] || [ "$new_gateway" = "$old_gateway" ] || printf 'owned-gateway-route %s\n' "$new_gateway"
        printf 'desired-address %s\n' "$new_address"
        [ -z "$new_router" ] || printf 'desired-router %s\n' "$new_router"
        [ -z "$new_gateway" ] || printf 'desired-gateway-route %s\n' "$new_gateway"
    ) > "$tmp" || {
        $DN_BUSYBOX rm -f "$tmp"
        return 1
    }
    $DN_BUSYBOX chmod 0600 "$tmp" && $DN_BUSYBOX mv -f "$tmp" "$DN_PENDING_FILE" || {
        $DN_BUSYBOX rm -f "$tmp"
        return 1
    }
}

dn_load_pending() {
    local key value extra
    DN_PENDING_PRESENT=0
    DN_PENDING_ADDRESSES=
    DN_PENDING_ROUTERS=
    DN_PENDING_GATEWAYS=
    [ -e "$DN_PENDING_FILE" ] || return 0
    [ -f "$DN_PENDING_FILE" ] && [ ! -L "$DN_PENDING_FILE" ] || return 1
    while IFS=' ' read -r key value extra; do
        [ -n "$key" ] || continue
        [ -n "$value" ] && [ -z "$extra" ] || return 1
        case "$key" in
            owned-address | desired-address)
                dn_valid_cidr "$value" || return 1
                [ "$key" != owned-address ] || DN_PENDING_ADDRESSES="${DN_PENDING_ADDRESSES}${DN_PENDING_ADDRESSES:+ }$value"
                ;;
            owned-router | desired-router)
                dn_valid_host_ipv4 "$value" || return 1
                [ "$key" != owned-router ] || DN_PENDING_ROUTERS="${DN_PENDING_ROUTERS}${DN_PENDING_ROUTERS:+ }$value"
                ;;
            owned-gateway-route | desired-gateway-route)
                dn_valid_cidr "$value" && [ "${value#*/}" = 32 ] || return 1
                [ "$key" != owned-gateway-route ] || DN_PENDING_GATEWAYS="${DN_PENDING_GATEWAYS}${DN_PENDING_GATEWAYS:+ }$value"
                ;;
            *) return 1 ;;
        esac
    done < "$DN_PENDING_FILE"
    DN_PENDING_PRESENT=1
    return 0
}

dn_ip_addr_add() {
    $DN_BUSYBOX ip -4 address add "$2" dev "$1"
}

dn_ip_addr_replace() {
    $DN_BUSYBOX ip -4 address replace "$2" dev "$1"
}

dn_ip_addr_del() {
    $DN_BUSYBOX ip -4 address del "$2" dev "$1"
}

dn_ip_addr_present() {
    local output
    output=$($DN_BUSYBOX ip -4 address show dev "$1" 2>/dev/null) || return 2
    printf '%s\n' "$output" | $DN_BUSYBOX grep -F -q "inet $2 "
}

dn_ip_addr_remove_owned() {
    dn_ip_addr_del "$1" "$2" && return 0
    dn_ip_addr_present "$1" "$2"
    [ "$?" -eq 1 ]
}

dn_ip_gateway_add() {
    $DN_BUSYBOX ip -4 route add "$2" dev "$1"
}

dn_ip_gateway_replace() {
    $DN_BUSYBOX ip -4 route replace "$2" dev "$1"
}

dn_ip_gateway_del() {
    $DN_BUSYBOX ip -4 route del "$2" dev "$1"
}

dn_ip_gateway_present() {
    local iface route display_route output old_ifs line
    iface=$1
    route=$2
    display_route=${route%/32}
    output=$($DN_BUSYBOX ip -4 route show 2>/dev/null) || return 2
    old_ifs=$IFS
    IFS='
'
    for line in $output; do
        IFS=$old_ifs
        set -- $line
        IFS='
'
        [ "$#" -ge 3 ] && { [ "$1" = "$route" ] || [ "$1" = "$display_route" ]; } &&
            [ "$2" = dev ] &&
            [ "$3" = "$iface" ] && {
            IFS=$old_ifs
            return 0
        }
    done
    IFS=$old_ifs
    return 1
}

dn_ip_gateway_key_present() {
    local iface route display_route output old_ifs line token previous
    iface=$1
    route=$2
    display_route=${route%/32}
    output=$($DN_BUSYBOX ip -4 route show 2>/dev/null) || return 2
    old_ifs=$IFS
    IFS='
'
    for line in $output; do
        IFS=$old_ifs
        set -- $line
        IFS='
'
        [ "$#" -ge 1 ] && { [ "$1" = "$route" ] || [ "$1" = "$display_route" ]; } || continue
        previous=
        for token in "$@"; do
            if [ "$previous" = dev ] && [ "$token" = "$iface" ]; then
                IFS=$old_ifs
                return 0
            fi
            previous=$token
        done
    done
    IFS=$old_ifs
    return 1
}

dn_ip_gateway_remove_owned() {
    local presence
    dn_ip_gateway_present "$1" "$2"
    presence=$?
    case "$presence" in
        0)
            dn_ip_gateway_del "$1" "$2" && return 0
            dn_ip_gateway_present "$1" "$2"
            [ "$?" -eq 1 ]
            ;;
        1)
            # DragonOS currently removes routes by destination/interface key.
            # Refuse to delete when that key now belongs to a different route.
            dn_ip_gateway_key_present "$1" "$2"
            [ "$?" -eq 1 ]
            ;;
        *) return 1 ;;
    esac
}

dn_ip_default_add() {
    $DN_BUSYBOX ip -4 route add default via "$2" dev "$1"
}

dn_ip_default_replace() {
    $DN_BUSYBOX ip -4 route replace default via "$2" dev "$1"
}

dn_ip_default_del() {
    $DN_BUSYBOX ip -4 route del default via "$2" dev "$1"
}

dn_ip_default_present() {
    local iface gateway output old_ifs line token previous
    iface=$1
    gateway=$2
    output=$($DN_BUSYBOX ip -4 route show 2>/dev/null) || return 2
    old_ifs=$IFS
    IFS='
'
    for line in $output; do
        IFS=$old_ifs
        set -- $line
        IFS='
'
        [ "$#" -ge 1 ] && { [ "$1" = default ] || [ "$1" = 0.0.0.0/0 ]; } || continue
        if [ -n "$gateway" ] && [ "$#" -ge 5 ] && [ "$2" = via ] &&
            [ "$3" = "$gateway" ] && [ "$4" = dev ] && [ "$5" = "$iface" ]; then
            IFS=$old_ifs
            return 0
        fi
        previous=
        for token in "$@"; do
            if [ -z "$gateway" ] && [ "$previous" = dev ] && [ "$token" = "$iface" ]; then
                IFS=$old_ifs
                return 0
            fi
            previous=$token
        done
    done
    IFS=$old_ifs
    return 1
}

dn_ip_default_remove_owned() {
    local presence
    dn_ip_default_present "$1" "$2"
    presence=$?
    case "$presence" in
        0)
            dn_ip_default_del "$1" "$2" && return 0
            dn_ip_default_present "$1" "$2"
            [ "$?" -eq 1 ]
            ;;
        1)
            # An exact miss is idempotent only when the coarse route key is
            # also absent; otherwise it is an externally replaced default.
            dn_ip_default_present "$1" ""
            [ "$?" -eq 1 ]
            ;;
        *) return 1 ;;
    esac
}

dn_restore_committed() {
    local iface address router gateway_route ok
    iface=$1
    address=$2
    router=$3
    gateway_route=$4
    ok=0
    [ -z "$address" ] || dn_ip_addr_replace "$iface" "$address" || ok=1
    [ -z "$gateway_route" ] || dn_ip_gateway_replace "$iface" "$gateway_route" || ok=1
    [ -z "$router" ] || dn_ip_default_replace "$iface" "$router" || ok=1
    return "$ok"
}

dn_cleanup_pending_defaults() {
    local iface routers router presence
    iface=$1
    routers=$2
    [ -n "$routers" ] || return 0
    # All recorded defaults share DragonOS's coarse destination/interface key.
    # During recovery an exact miss for old may simply mean that new currently
    # occupies the key, so defer the unknown-owner decision until every WAL
    # identity has been checked.
    for router in $routers; do
        dn_ip_default_present "$iface" "$router"
        presence=$?
        case "$presence" in
            0) dn_ip_default_del "$iface" "$router" || return 1 ;;
            1) ;;
            *) return 1 ;;
        esac
    done
    dn_ip_default_present "$iface" ""
    [ "$?" -eq 1 ]
}

dn_cleanup_objects() {
    local iface addresses routers gateways value ok
    iface=$1
    addresses=$2
    routers=$3
    gateways=$4
    ok=0
    dn_cleanup_pending_defaults "$iface" "$routers" >/dev/null 2>&1 || ok=1
    for value in $gateways; do dn_ip_gateway_remove_owned "$iface" "$value" >/dev/null 2>&1 || ok=1; done
    for value in $addresses; do dn_ip_addr_remove_owned "$iface" "$value" >/dev/null 2>&1 || ok=1; done
    return "$ok"
}

dn_recover_pending_locked() {
    local iface old_present old_address old_router old_gateway
    iface=$1
    dn_load_pending || {
        dn_write_error "$iface" "invalid pending transaction state"
        return 1
    }
    [ "$DN_PENDING_PRESENT" -eq 1 ] || return 0
    dn_load_lease "$DN_LEASE_FILE" || {
        dn_write_error "$iface" "invalid committed lease during recovery"
        return 1
    }
    old_present=$DN_LEASE_PRESENT
    old_address=$DN_LEASE_ADDRESS
    old_router=$DN_LEASE_ROUTER
    old_gateway=$DN_LEASE_GATEWAY_ROUTE

    dn_cleanup_objects "$iface" "$DN_PENDING_ADDRESSES" "$DN_PENDING_ROUTERS" "$DN_PENDING_GATEWAYS" || {
        dn_write_error "$iface" "failed to clean pending transaction objects"
        return 1
    }
    if [ "$old_present" -eq 1 ]; then
        dn_restore_committed "$iface" "$old_address" "$old_router" "$old_gateway" || {
            dn_write_error "$iface" "failed to restore committed lease during recovery"
            return 1
        }
    fi
    $DN_BUSYBOX rm -f "$DN_PENDING_FILE" || return 1
    return 0
}

dn_rebuild_dns_locked() {
    local iface_dir lease value seen_dns seen_search dns_count search_line search_total tmp restore_noglob
    seen_dns=" "
    seen_search=" "
    dns_count=0
    search_line=
    search_total=0
    dn_require_config_dir_chain / /etc || {
        dn_log dns "/etc directory chain is not trusted"
        return 1
    }
    if [ -e "$DN_RESOLV_CONF" ] || [ -L "$DN_RESOLV_CONF" ]; then
        [ -f "$DN_RESOLV_CONF" ] && [ ! -L "$DN_RESOLV_CONF" ] || {
            dn_log dns "$DN_RESOLV_CONF must be a regular file"
            return 1
        }
    fi
    tmp="$DN_RESOLV_CONF.tmp.$$"
    [ ! -L "$tmp" ] || return 1
    : > "$tmp" || return 1

    restore_noglob=0
    case $- in
        *f*)
            restore_noglob=1
            set +f
            ;;
    esac
    set -- "$DN_INTERFACES_RUN"/*
    [ "$restore_noglob" -eq 0 ] || set -f
    for iface_dir in "$@"; do
        [ -d "$iface_dir" ] && [ ! -L "$iface_dir" ] || continue
        lease="$iface_dir/lease"
        [ -f "$lease" ] && [ ! -L "$lease" ] || continue
        dn_load_lease "$lease" || {
            $DN_BUSYBOX rm -f "$tmp"
            return 1
        }
        for value in $DN_LEASE_SEARCH; do
            case "$seen_search" in *" $value "*) continue ;; esac
            [ "$((search_total + ${#value} + 1))" -le 256 ] || continue
            seen_search="$seen_search$value "
            search_line="${search_line}${search_line:+ }$value"
            search_total=$((search_total + ${#value} + 1))
        done
        for value in $DN_LEASE_DNS; do
            [ "$dns_count" -lt 3 ] || break
            case "$seen_dns" in *" $value "*) continue ;; esac
            seen_dns="$seen_dns$value "
            printf 'nameserver %s\n' "$value" >> "$tmp" || {
                $DN_BUSYBOX rm -f "$tmp"
                return 1
            }
            dns_count=$((dns_count + 1))
        done
    done

    if [ -n "$search_line" ]; then
        {
            printf 'search %s\n' "$search_line"
            $DN_BUSYBOX cat "$tmp"
        } > "$tmp.search" || {
            $DN_BUSYBOX rm -f "$tmp" "$tmp.search"
            return 1
        }
        $DN_BUSYBOX mv -f "$tmp.search" "$tmp" || {
            $DN_BUSYBOX rm -f "$tmp" "$tmp.search"
            return 1
        }
    fi
    $DN_BUSYBOX chmod 0644 "$tmp" && $DN_BUSYBOX mv -f "$tmp" "$DN_RESOLV_CONF" || {
        $DN_BUSYBOX rm -f "$tmp" "$tmp.search"
        return 1
    }
}

dn_rebuild_dns() {
    local lock result
    lock="$DN_RUN/dns.lock"
    [ ! -L "$lock" ] || return 1
    : > "$lock" || return 1
    $DN_BUSYBOX chmod 0600 "$lock" || return 1
    exec 8>"$lock" || return 1
    $DN_BUSYBOX flock -x 8 || {
        exec 8>&-
        return 1
    }
    dn_rebuild_dns_locked
    result=$?
    exec 8>&-
    return "$result"
}

dn_rollback_transaction() {
    local iface old_present old_address old_router old_gateway new_address new_router new_gateway
    iface=$1
    old_present=$2
    old_address=$3
    old_router=$4
    old_gateway=$5
    new_address=$6
    new_router=$7
    new_gateway=$8

    [ "${DN_TX_NEW_ROUTER_APPLIED:-0}" -eq 0 ] || dn_ip_default_remove_owned "$iface" "$new_router" >/dev/null 2>&1 || return 1
    [ "${DN_TX_NEW_GATEWAY_APPLIED:-0}" -eq 0 ] || dn_ip_gateway_remove_owned "$iface" "$new_gateway" >/dev/null 2>&1 || return 1
    [ "${DN_TX_NEW_ADDRESS_APPLIED:-0}" -eq 0 ] || dn_ip_addr_remove_owned "$iface" "$new_address" >/dev/null 2>&1 || return 1
    if [ "$old_present" -eq 1 ]; then
        dn_restore_committed "$iface" "$old_address" "$old_router" "$old_gateway" || return 1
    fi
    $DN_BUSYBOX rm -f "$DN_PENDING_FILE" || return 1
    return 0
}

dn_apply_locked() {
    local iface owner new_address new_router new_gateway new_dns new_search
    local old_present old_address old_router old_gateway same_network failed presence
    iface=$1
    owner=$2
    new_address=$3
    new_router=$4
    new_gateway=$5
    new_dns=$6
    new_search=$7

    dn_recover_pending_locked "$iface" || return 1
    dn_load_lease "$DN_LEASE_FILE" || {
        dn_write_error "$iface" "invalid committed lease state"
        return 1
    }
    old_present=$DN_LEASE_PRESENT
    old_address=$DN_LEASE_ADDRESS
    old_router=$DN_LEASE_ROUTER
    old_gateway=$DN_LEASE_GATEWAY_ROUTE

    same_network=0
    if [ "$old_present" -eq 1 ] && [ "$old_address" = "$new_address" ] &&
        [ "$old_router" = "$new_router" ] && [ "$old_gateway" = "$new_gateway" ]; then
        same_network=1
        dn_ip_addr_present "$iface" "$new_address" || same_network=0
        [ -z "$new_gateway" ] || dn_ip_gateway_present "$iface" "$new_gateway" || same_network=0
        [ -z "$new_router" ] || dn_ip_default_present "$iface" "$new_router" || same_network=0
    fi

    if [ "$same_network" -eq 1 ]; then
        dn_write_lease "$owner" "$new_address" "$new_router" "$new_gateway" "$new_dns" "$new_search" || return 1
        if ! dn_rebuild_dns; then
            dn_write_error "$iface" "lease committed but managed DNS rebuild failed"
            return 1
        fi
        dn_clear_error
        return 0
    fi

    # A pending journal must name every object that a crash may leave behind.
    # Before recording a previously unowned object, prove that it is absent so
    # recovery can never delete an identical object owned by somebody else.
    if [ "$new_address" != "$old_address" ]; then
        dn_ip_addr_present "$iface" "$new_address"
        presence=$?
        case "$presence" in
            0)
                dn_write_error "$iface" "desired address already exists outside service ownership"
                return 1
                ;;
            1) ;;
            *)
                dn_write_error "$iface" "failed to query desired address before transaction"
                return 1
                ;;
        esac
    fi
    if [ -n "$old_gateway" ]; then
        dn_ip_gateway_present "$iface" "$old_gateway"
        presence=$?
        case "$presence" in
            0) ;;
            1)
                dn_ip_gateway_key_present "$iface" "$old_gateway"
                presence=$?
                case "$presence" in
                    0)
                        dn_write_error "$iface" "owned gateway route was replaced outside service ownership"
                        return 1
                        ;;
                    1) ;;
                    *)
                        dn_write_error "$iface" "failed to query owned gateway route key before transaction"
                        return 1
                        ;;
                esac
                ;;
            *)
                dn_write_error "$iface" "failed to query owned gateway route before transaction"
                return 1
                ;;
        esac
    fi
    if [ -n "$new_gateway" ] && [ "$new_gateway" != "$old_gateway" ]; then
        dn_ip_gateway_key_present "$iface" "$new_gateway"
        presence=$?
        case "$presence" in
            0)
                dn_write_error "$iface" "desired gateway route already exists outside service ownership"
                return 1
                ;;
            1) ;;
            *)
                dn_write_error "$iface" "failed to query desired gateway route before transaction"
                return 1
                ;;
        esac
    fi
    if [ -n "$old_router" ]; then
        # A committed lease is ownership evidence only for the exact old
        # route. Validate it even when this transaction removes the default,
        # otherwise rollback could replace an external same-key route.
        dn_ip_default_present "$iface" "$old_router"
        presence=$?
        case "$presence" in
            0) ;;
            1)
                dn_ip_default_present "$iface" ""
                presence=$?
                case "$presence" in
                    0)
                        dn_write_error "$iface" "default route was replaced outside service ownership"
                        return 1
                        ;;
                    1) ;;
                    *)
                        dn_write_error "$iface" "failed to query default route before transaction"
                        return 1
                        ;;
                esac
                ;;
            *)
                dn_write_error "$iface" "failed to query owned default route before transaction"
                return 1
                ;;
        esac
    elif [ -n "$new_router" ]; then
        dn_ip_default_present "$iface" ""
        presence=$?
        case "$presence" in
            0)
                dn_write_error "$iface" "default route already exists outside service ownership"
                return 1
                ;;
            1) ;;
            *)
                dn_write_error "$iface" "failed to query default route before transaction"
                return 1
                ;;
        esac
    fi

    dn_write_pending "$old_address" "$old_router" "$old_gateway" "$new_address" "$new_router" "$new_gateway" || {
        dn_write_error "$iface" "failed to prepare pending transaction"
        return 1
    }

    DN_TX_ACTIVE=1
    DN_TX_IFACE=$iface
    DN_TX_OLD_PRESENT=$old_present
    DN_TX_OLD_ADDRESS=$old_address
    DN_TX_OLD_ROUTER=$old_router
    DN_TX_OLD_GATEWAY=$old_gateway
    DN_TX_NEW_ADDRESS=$new_address
    DN_TX_NEW_ROUTER=$new_router
    DN_TX_NEW_GATEWAY=$new_gateway
    DN_TX_NEW_ADDRESS_APPLIED=0
    DN_TX_NEW_ROUTER_APPLIED=0
    DN_TX_NEW_GATEWAY_APPLIED=0
    trap 'dn_transaction_signal' HUP INT TERM

    failed=0
    if [ "$old_address" = "$new_address" ]; then
        dn_ip_addr_replace "$iface" "$new_address" || failed=1
    else
        DN_TX_NEW_ADDRESS_APPLIED=1
        dn_ip_addr_add "$iface" "$new_address" || {
            DN_TX_NEW_ADDRESS_APPLIED=0
            failed=1
        }
    fi

    if [ "$failed" -eq 0 ] && [ -n "$new_gateway" ]; then
        if [ "$new_gateway" = "$old_gateway" ]; then
            dn_ip_gateway_replace "$iface" "$new_gateway" || failed=1
        else
            DN_TX_NEW_GATEWAY_APPLIED=1
            dn_ip_gateway_add "$iface" "$new_gateway" || {
                DN_TX_NEW_GATEWAY_APPLIED=0
                failed=1
            }
        fi
    fi

    if [ "$failed" -eq 0 ]; then
        if [ -n "$new_router" ]; then
            if [ -n "$old_router" ]; then
                if [ "$new_router" = "$old_router" ]; then
                    dn_ip_default_replace "$iface" "$new_router" || failed=1
                else
                    DN_TX_NEW_ROUTER_APPLIED=1
                    dn_ip_default_replace "$iface" "$new_router" || {
                        DN_TX_NEW_ROUTER_APPLIED=0
                        failed=1
                    }
                fi
            else
                DN_TX_NEW_ROUTER_APPLIED=1
                dn_ip_default_add "$iface" "$new_router" || {
                    DN_TX_NEW_ROUTER_APPLIED=0
                    failed=1
                }
            fi
        elif [ -n "$old_router" ]; then
            dn_ip_default_remove_owned "$iface" "$old_router" || failed=1
        fi
    fi

    if [ "$failed" -eq 0 ] && [ -n "$old_gateway" ] && [ "$old_gateway" != "$new_gateway" ]; then
        dn_ip_gateway_remove_owned "$iface" "$old_gateway" || failed=1
    fi
    if [ "$failed" -eq 0 ] && [ -n "$old_address" ] && [ "$old_address" != "$new_address" ]; then
        dn_ip_addr_remove_owned "$iface" "$old_address" || failed=1
    fi

    if [ "$failed" -ne 0 ]; then
        trap - HUP INT TERM
        DN_TX_ACTIVE=0
        if dn_rollback_transaction "$iface" "$old_present" "$old_address" "$old_router" "$old_gateway" "$new_address" "$new_router" "$new_gateway"; then
            dn_write_error "$iface" "network transaction failed and was rolled back"
        else
            dn_write_error "$iface" "network transaction failed; pending recovery retained"
        fi
        return 1
    fi

    if ! dn_write_lease "$owner" "$new_address" "$new_router" "$new_gateway" "$new_dns" "$new_search"; then
        trap - HUP INT TERM
        DN_TX_ACTIVE=0
        dn_rollback_transaction "$iface" "$old_present" "$old_address" "$old_router" "$old_gateway" "$new_address" "$new_router" "$new_gateway" || :
        dn_write_error "$iface" "failed to commit lease state"
        return 1
    fi
    $DN_BUSYBOX rm -f "$DN_PENDING_FILE" || {
        trap - HUP INT TERM
        DN_TX_ACTIVE=0
        dn_write_error "$iface" "lease committed; stale pending journal requires recovery"
        return 1
    }
    trap - HUP INT TERM
    DN_TX_ACTIVE=0

    if ! dn_rebuild_dns; then
        dn_write_error "$iface" "lease committed but managed DNS rebuild failed"
        return 1
    fi
    dn_clear_error
    return 0
}

dn_transaction_signal() {
    trap - HUP INT TERM
    if [ "${DN_TX_ACTIVE:-0}" -eq 1 ]; then
        # Once the atomic lease rename is visible, the new network is committed.
        # Leave the WAL for the next entry point instead of rolling it back.
        if dn_load_lease "$DN_LEASE_FILE" && [ "$DN_LEASE_PRESENT" -eq 1 ] &&
            [ "$DN_LEASE_ADDRESS" = "$DN_TX_NEW_ADDRESS" ] &&
            [ "$DN_LEASE_ROUTER" = "$DN_TX_NEW_ROUTER" ] &&
            [ "$DN_LEASE_GATEWAY_ROUTE" = "$DN_TX_NEW_GATEWAY" ]; then
            exit 1
        fi
        dn_rollback_transaction "$DN_TX_IFACE" "$DN_TX_OLD_PRESENT" "$DN_TX_OLD_ADDRESS" \
            "$DN_TX_OLD_ROUTER" "$DN_TX_OLD_GATEWAY" "$DN_TX_NEW_ADDRESS" \
            "$DN_TX_NEW_ROUTER" "$DN_TX_NEW_GATEWAY" || :
    fi
    exit 1
}

dn_apply() {
    local iface result
    iface=$1
    dn_init_runtime && dn_init_interface_runtime "$iface" && dn_acquire_state_lock || return 1
    dn_apply_locked "$@"
    result=$?
    exec 7>&-
    return "$result"
}

dn_apply_active_dhcp() {
    local iface result
    iface=$1
    dn_init_runtime && dn_init_interface_runtime "$iface" && dn_acquire_state_lock || return 1
    if ! dn_require_active_dhcp_policy; then
        dn_write_error "$iface" "DHCP event rejected because the active policy is not dhcp"
        result=1
    else
        # Policy validation and lease commit share state.lock. A callback that
        # loses a race with stop/static/unmanaged therefore cannot resurrect
        # an obsolete DHCP address, route, or resolver state.
        dn_apply_locked "$@"
        result=$?
    fi
    exec 7>&-
    return "$result"
}

dn_deconfigure_locked() {
    local iface addresses routers gateways cleanup_failed value
    iface=$1
    dn_recover_pending_locked "$iface" || return 1
    dn_load_lease "$DN_LEASE_FILE" || {
        dn_write_error "$iface" "invalid committed lease state"
        return 1
    }
    [ "$DN_LEASE_PRESENT" -eq 1 ] || {
        dn_rebuild_dns || return 1
        dn_clear_error
        return 0
    }

    addresses=$DN_LEASE_ADDRESS
    routers=$DN_LEASE_ROUTER
    gateways=$DN_LEASE_GATEWAY_ROUTE
    cleanup_failed=0
    for value in $routers; do dn_ip_default_remove_owned "$iface" "$value" >/dev/null 2>&1 || cleanup_failed=1; done
    for value in $gateways; do dn_ip_gateway_remove_owned "$iface" "$value" >/dev/null 2>&1 || cleanup_failed=1; done
    for value in $addresses; do dn_ip_addr_remove_owned "$iface" "$value" >/dev/null 2>&1 || cleanup_failed=1; done
    if [ "$cleanup_failed" -ne 0 ]; then
        dn_write_error "$iface" "deconfig could not remove every owned object; lease retained"
        return 1
    fi
    $DN_BUSYBOX rm -f "$DN_LEASE_FILE" || return 1
    if ! dn_rebuild_dns; then
        dn_write_error "$iface" "network deconfigured but managed DNS rebuild failed"
        return 1
    fi
    dn_clear_error
    return 0
}

dn_deconfigure() {
    local iface result
    iface=$1
    dn_init_runtime && dn_init_interface_runtime "$iface" && dn_acquire_state_lock || return 1
    dn_deconfigure_locked "$iface"
    result=$?
    exec 7>&-
    return "$result"
}
