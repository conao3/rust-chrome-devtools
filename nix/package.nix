{
  lib,
  rustPlatform,
  makeWrapper,
  runtimePackages,
  chromeDevtoolsMcp,
}:

let
  source = lib.cleanSource ../.;
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = "chrome-devtools";
  version = cargoToml.package.version;
  src = source;

  cargoLock.lockFile = "${source}/Cargo.lock";

  postPatch = ''
    patchShebangs tests/fixtures
  '';

  cargoTestFlags = [ "--bins" ];
  nativeBuildInputs = [ makeWrapper ];

  postInstall = ''
    wrapProgram "$out/bin/chrome-devtools" \
      --set-default CHROME_DEVTOOLS_MCP_COMMAND ${lib.getExe chromeDevtoolsMcp} \
      --prefix PATH : ${lib.makeBinPath runtimePackages}
  '';

  meta = {
    description = "Profile-aware CLI for running Chrome DevTools MCP with isolated Chrome user data directories";
    homepage = "https://github.com/conao3/rust-chrome-devtools";
    license = lib.licenses.asl20;
    mainProgram = "chrome-devtools";
    platforms = lib.platforms.unix;
  };
}
