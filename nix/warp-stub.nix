{
  writeShellApplication,
  symlinkJoin,
  coreutils,
  iproute2,
  jq,
  socat,
}:
# Stands in for Cloudflare WARP in VM tests: warp-cli records the requested state in the
# instance's daemon directory, and warp-svc serves its socket and a proxy port that answers "stub".
let
  warp-svc = writeShellApplication {
    name = "warp-svc";
    runtimeInputs = [
      coreutils
      iproute2
      socat
    ];
    text = ''
      # Behaves like one process: stopping it stops every listener it started.
      trap 'kill 0' EXIT
      # Bridge access routes into the link warp-svc creates in warp mode, whose MTU is smaller
      # than the host link's.
      ip link add CloudflareWARP mtu 1280 type dummy 2>/dev/null && ip link set CloudflareWARP up || true
      echo Disconnected > /run/cloudflare-warp/state
      socat TCP-LISTEN:40000,bind=127.0.0.1,fork,reuseaddr SYSTEM:'echo stub' &
      socat UNIX-LISTEN:/run/cloudflare-warp/warp_service,fork,unlink-early EXEC:true &
      wait
    '';
  };
  warp-cli = writeShellApplication {
    name = "warp-cli";
    runtimeInputs = [
      coreutils
      iproute2
      jq
    ];
    text = ''
      state=/run/cloudflare-warp/state
      arguments=" $* "
      case "$arguments" in
        *" --listen status "*)
          previous=""
          while true; do
            current=$(cat "$state" 2>/dev/null || echo Disconnected)
            if [ "$current" != "$previous" ]; then
              case "$current" in
                Connected) echo '{"status":"Connected","reason":"NetworkHealthy"}' ;;
                *) echo '{"status":"Disconnected","reason":"Manual"}' ;;
              esac
              previous=$current
            fi
            sleep 0.1
          done
          ;;
        *" registration new "*)
          echo '{}' > /var/lib/cloudflare-warp/reg.json
          echo '{"account":{"account_type":"free"},"endpoints":[],"interface":{"v4":"172.16.0.2","v6":"2001:db8::2"}}' > /var/lib/cloudflare-warp/conf.json
          ;;
        *" settings "*) echo '{"settings":{"operation_mode":"warp","warp_tunnel_protocol":"MASQUE"}}' ;;
        *" tunnel stats "*) echo '{"warp_is_on":true,"protocol":"MASQUE","edge":{"colo":"TST"}}' ;;
        *" disconnect "*) echo Disconnected > "$state" ;;
        *" connect "*)
          # Like warp-svc, recreate the link on every connect, dropping the routes through it.
          ip link del CloudflareWARP 2>/dev/null || true
          ip link add CloudflareWARP mtu 1280 type dummy && ip link set CloudflareWARP up
          conf=/var/lib/cloudflare-warp/conf.json
          v4=$(jq -r '.interface.v4 // "172.16.0.2"' "$conf" 2>/dev/null || echo 172.16.0.2)
          v6=$(jq -r '.interface.v6 // "2001:db8::2"' "$conf" 2>/dev/null || echo 2001:db8::2)
          ip address add "$v4/32" dev CloudflareWARP
          ip -6 address add "$v6/128" dev CloudflareWARP nodad
          # A shared connector address must never be used as the SNAT target.
          ip -6 address add 2001:db8::1/128 dev CloudflareWARP preferred_lft 0 nodad
          echo Connected > "$state"
          ;;
        *) ;;
      esac
    '';
  };
in
symlinkJoin {
  name = "cloudflare-warp-stub";
  paths = [
    warp-svc
    warp-cli
  ];
}
