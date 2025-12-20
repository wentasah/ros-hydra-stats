with import <nixpkgs> {};
mkShell {
  packages = [
    openssl
    pkg-config
  ];
}
