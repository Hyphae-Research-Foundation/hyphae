// SPDX-License-Identifier: Apache-2.0
// dl.hyphae.dev — Hyphae distribution front door.
// Serves the pinned installer script and stable redirects; every binary
// byte still comes from GitHub Releases with its provenance.
const INSTALL_B64 = "";
const REPO = "https://github.com/Hyphae-Research-Foundation/hyphae";

function installScript() {
  const bytes = Uint8Array.from(atob(INSTALL_B64), (c) => c.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname.replace(/\/+$/, "") || "/";
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("method not allowed\n", { status: 405 });
    }
    switch (path) {
      case "/":
      case "/install":
      case "/install.sh":
        return new Response(installScript(), {
          headers: {
            "content-type": "text/x-shellscript; charset=utf-8",
            "cache-control": "public, max-age=300",
          },
        });
      case "/latest":
        return Response.redirect(`${REPO}/releases/latest`, 302);
      case "/releases":
        return Response.redirect(`${REPO}/releases`, 302);
      case "/aur":
        return Response.redirect("https://github.com/Hyphae-Research-Foundation/hyphae-aur", 302);
      case "/sums":
        return Response.redirect(`${REPO}/releases/latest/download/SHA256SUMS`, 302);
      default:
        return Response.redirect(REPO, 302);
    }
  },
};
