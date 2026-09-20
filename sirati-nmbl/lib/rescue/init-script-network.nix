# Rescue /init — signed DHCP or static network-stage application.
{
  bash,
  coreutils,
  dhcpcd,
  gawk,
  iproute2,
}:
''
  ${""}    # --- networking ---
      local_network_only() {
        log "ERROR: $*; networking and sshd disabled; local console only"
        exec ${bash}/bin/bash -i < /dev/console > /dev/console 2>&1
      }

      if [ -e /etc/nmbl-network-disabled ]; then
        local_network_only "signed network stage unavailable"
      fi

      network_config=/nmbl-network/etc/nmbl-network/network.conf
      [ -r "$network_config" ] || local_network_only "network profile is missing"
      network_version=$(${gawk}/bin/awk 'NF { if ($1 == "version") { print $2; exit } }' "$network_config")
      network_family=$(${gawk}/bin/awk 'NF { if ($1 == "address-family") { print $2; exit } }' "$network_config")
      ${iproute2}/bin/ip link set lo up > /dev/console 2>&1 || true

      if [ "$network_version" = 2 ]; then
        log "applying signed static network profiles"
        : > /etc/resolv.conf || local_network_only "cannot create resolv.conf"
        current_iface=""
        ifaces=""
        while read -r directive one two three four extra; do
          [ -z "$directive" ] && continue
          [ -z "$extra" ] || local_network_only "unexpected static profile fields"
          case "$directive" in
            version|address-family) ;;
            dns)
              [ -n "$one" ] && [ -z "$two" ] || local_network_only "malformed DNS directive"
              ${coreutils}/bin/printf 'nameserver %s\n' "$one" >> /etc/resolv.conf \
                || local_network_only "cannot write resolv.conf"
              ;;
            profile)
              [ -z "$current_iface" ] || local_network_only "nested static profile"
              case "$one" in
                interface)
                  [ -d "/sys/class/net/$two" ] || local_network_only "configured interface $two is absent"
                  current_iface="$two"
                  ;;
                mac)
                  for candidate_path in /sys/class/net/*; do
                    candidate=$(${coreutils}/bin/basename "$candidate_path")
                    [ "$candidate" = lo ] && continue
                    candidate_mac=$(${coreutils}/bin/tr 'A-F' 'a-f' < "$candidate_path/address" 2>/dev/null || true)
                    if [ "$candidate_mac" = "$two" ]; then current_iface="$candidate"; break; fi
                  done
                  [ -n "$current_iface" ] || local_network_only "configured MAC $two is absent"
                  ;;
                *) local_network_only "unknown interface selector" ;;
              esac
              case " $ifaces " in *" $current_iface "*) local_network_only "interface $current_iface selected twice";; esac
              ${iproute2}/bin/ip link set dev "$current_iface" up > /dev/console 2>&1 \
                || local_network_only "could not bring up $current_iface"
              ifaces="$ifaces $current_iface"
              ;;
            address)
              [ -n "$current_iface" ] || local_network_only "address outside a profile"
              ${iproute2}/bin/ip -"$one" address replace "$two" dev "$current_iface" > /dev/console 2>&1 \
                || local_network_only "could not apply IPv$one address $two"
              ;;
            gateway)
              [ -n "$current_iface" ] || local_network_only "gateway outside a profile"
              if [ "$three" = onlink ]; then
                ${iproute2}/bin/ip -"$one" route replace default via "$two" dev "$current_iface" onlink
              else
                ${iproute2}/bin/ip -"$one" route replace default via "$two" dev "$current_iface"
              fi > /dev/console 2>&1 || local_network_only "could not apply IPv$one gateway $two"
              ;;
            route)
              [ -n "$current_iface" ] || local_network_only "route outside a profile"
              if [ "$three" = - ]; then
                if [ "$four" = onlink ]; then
                  ${iproute2}/bin/ip -"$one" route replace "$two" dev "$current_iface" onlink
                else
                  ${iproute2}/bin/ip -"$one" route replace "$two" dev "$current_iface"
                fi
              elif [ "$four" = onlink ]; then
                ${iproute2}/bin/ip -"$one" route replace "$two" via "$three" dev "$current_iface" onlink
              else
                ${iproute2}/bin/ip -"$one" route replace "$two" via "$three" dev "$current_iface"
              fi > /dev/console 2>&1 || local_network_only "could not apply IPv$one route $two"
              ;;
            end)
              [ -n "$current_iface" ] || local_network_only "end outside a profile"
              current_iface=""
              ;;
            *) local_network_only "unknown network directive $directive" ;;
          esac
        done < "$network_config"
        [ -z "$current_iface" ] || local_network_only "unterminated static profile"
        [ -n "$ifaces" ] || local_network_only "static profile selected no interfaces"
        log "signed static network configuration applied"
      else
        log "bringing up rescue networking with DHCP"
        configured_ifaces=""
        while read -r directive value extra; do
          [ "$directive" != interface ] || configured_ifaces="$configured_ifaces $value"
        done < "$network_config"
        candidates="$configured_ifaces"
        [ -n "$candidates" ] || candidates=$(${coreutils}/bin/ls /sys/class/net 2>/dev/null)
        ifaces=""
      for iface in $candidates; do
        [ "$iface" = lo ] && continue
        [ -d "/sys/class/net/$iface" ] || continue
        ${iproute2}/bin/ip link set dev "$iface" up > /dev/console 2>&1 || true
        ifaces="$ifaces $iface"
      done
      [ -n "$ifaces" ] || local_network_only "DHCP profile selected no interfaces"
      carrier=""
      for _ in $(${coreutils}/bin/seq 1 40); do
        for iface in $ifaces; do
          if [ "$(${coreutils}/bin/cat "/sys/class/net/$iface/carrier" 2>/dev/null || echo 0)" = 1 ]; then
            carrier="$iface"
            break 2
          fi
        done
        ${coreutils}/bin/sleep 0.5
      done
      [ -n "$carrier" ] || log "WARNING: no carrier after 20 seconds"
      ${coreutils}/bin/mkdir -p /var/lib/dhcpcd /var/run/dhcpcd /run/dhcpcd
        ${coreutils}/bin/touch /etc/dhcpcd.conf 2>/dev/null || true
        case "$network_family" in
          ipv4-only) family_args="-4 --waitip=4" ;;
          ipv6-only) family_args="-6 --waitip=6" ;;
          dual-stack) family_args="--waitip" ;;
          *) local_network_only "invalid DHCP address family" ;;
        esac
        ${dhcpcd}/bin/dhcpcd $family_args -t 20 $ifaces > /dev/console 2>&1 \
          || log "WARNING: dhcpcd did not bind an address in time"
      fi

      log "rescue addresses and routes:"
      ${iproute2}/bin/ip address > /dev/console 2>&1 || true
      ${iproute2}/bin/ip route show table all > /dev/console 2>&1 || true
''
