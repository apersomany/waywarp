{
  runCommand,
  runtimeShell,
  writeShellScript,
  bash,
  coreutils,
  gnugrep,
}:
# Runs install.sh against a fixture release. Stub curl and uname stand in for GitHub and the host,
# so the check needs no network and never writes outside its build directory.
let
  curl = writeShellScript "curl" ''
    # Called as: curl -fsSL -o OUTPUT URL
    output=$3
    file=$RELEASE/''${4##*/}
    [ -f "$file" ] || exit 22
    cp "$file" "$output"
  '';
  uname = writeShellScript "uname" ''
    case $1 in
      -s) echo Linux ;;
      -m) echo "$MACHINE" ;;
    esac
  '';
in
runCommand "waywarp-installer" { } ''
  export PATH=$PWD/stubs:${bash}/bin:${coreutils}/bin:${gnugrep}/bin
  mkdir stubs
  ln -s ${curl} stubs/curl
  ln -s ${uname} stubs/uname

  release() {
    rm -rf release && mkdir release
    printf '#!${runtimeShell}\necho "waywarp 0.1.0"\n' > release/waywarp-x86_64-linux
    (cd release && sha256sum waywarp-x86_64-linux > SHA256SUMS)
  }
  install() {
    RELEASE=$PWD/release MACHINE=$1 WAYWARP_PREFIX=$PWD/prefix sh ${../install.sh} >log 2>&1
  }
  expect_failure() {
    if install "$1"; then
      echo "installer succeeded unexpectedly"; cat log; exit 1
    fi
    grep -F "$2" log || { cat log; exit 1; }
  }

  release
  install x86_64 || { cat log; exit 1; }
  grep -F 'note: installed waywarp 0.1.0' log
  grep -F 'step: downloading waywarp-x86_64-linux' log
  grep -F 'warning: not on PATH:' log
  test -x prefix/bin/waywarp

  release
  echo tampered >> release/waywarp-x86_64-linux
  expect_failure x86_64 'checksum mismatch'

  release
  : > release/SHA256SUMS
  expect_failure x86_64 'checksum mismatch'

  release
  rm release/waywarp-x86_64-linux
  expect_failure x86_64 'cannot download waywarp-x86_64-linux'

  expect_failure riscv64 'no release for riscv64'

  touch $out
''
