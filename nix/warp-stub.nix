{
  writeShellApplication,
  symlinkJoin,
  coreutils,
  iproute2,
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
      # Bridge access routes into the link warp-svc creates in warp mode.
      ip link add CloudflareWARP type dummy 2>/dev/null && ip link set CloudflareWARP up || true
      echo Disconnected > /run/cloudflare-warp/state
      socat TCP-LISTEN:40000,bind=127.0.0.1,fork,reuseaddr SYSTEM:'echo stub' &
      socat UNIX-LISTEN:/run/cloudflare-warp/warp_service,fork,unlink-early EXEC:true &
      wait
    '';
  };
  warp-cli = writeShellApplication {
    name = "warp-cli";
    runtimeInputs = [ coreutils ];
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
        *" registration new "*) echo '{}' > /var/lib/cloudflare-warp/reg.json ;;
        *" settings "*) echo '{"settings":{"operation_mode":"warp","warp_tunnel_protocol":"MASQUE"}}' ;;
        *" tunnel stats "*) echo '{"warp_is_on":true,"protocol":"MASQUE","edge":{"colo":"TST"}}' ;;
        *" disconnect "*) echo Disconnected > "$state" ;;
        *" connect "*) echo Connected > "$state" ;;
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
