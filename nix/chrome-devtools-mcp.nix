{
  lib,
  stdenvNoCC,
  fetchzip,
  makeWrapper,
  nodejs,
}:

stdenvNoCC.mkDerivation rec {
  pname = "chrome-devtools-mcp";
  version = "1.8.0";

  src = fetchzip {
    name = "${pname}-${version}.tgz";
    url = "https://registry.npmjs.org/${pname}/-/${pname}-${version}.tgz";
    extension = "tar.gz";
    hash = "sha256-5072DGsHCLYb50BMUgtjSn/1l8Ua538yPwdS3Ldkjmo=";
  };

  nativeBuildInputs = [ makeWrapper ];

  # Upstream leaves Puppeteer's protocolTimeout at 180 seconds. A shared Chrome
  # can exceed that while another session runs a heavy CDP command, so align the
  # default with the broker's 300-second heavy-tool timeout. The environment
  # variable remains available for callers that need a different value.
  postPatch = ''
    substituteInPlace build/src/browser.js \
      --replace-fail 'const connectOptions = {' \
        'const connectOptions = {
        protocolTimeout: Number(process.env.CHROME_DEVTOOLS_MCP_PROTOCOL_TIMEOUT_MS ?? 300000),'
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib/${pname}
    cp -r . $out/lib/${pname}
    makeWrapper ${nodejs}/bin/node $out/bin/${pname} \
      --add-flags $out/lib/${pname}/build/src/bin/chrome-devtools-mcp.js
    runHook postInstall
  '';

  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    $out/bin/${pname} --version | grep -F ${version}
    runHook postInstallCheck
  '';

  meta = {
    description = "MCP server exposing Chrome DevTools capabilities to AI coding assistants";
    homepage = "https://github.com/ChromeDevTools/chrome-devtools-mcp";
    license = lib.licenses.asl20;
    mainProgram = pname;
    platforms = lib.platforms.unix;
  };
}
