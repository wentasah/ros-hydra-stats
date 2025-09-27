with import <nixpkgs> {};
mkShell {
  packages = [
    bashInteractive
    openssl
    pkg-config
  ];
}
