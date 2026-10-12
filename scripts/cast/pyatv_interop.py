#!/usr/bin/env python3
"""Dev-only AirPlay pairing interop between Tuner and pyatv 0.18.0 (DESIGN §40 §14.3, anchor 3).

Our client and our fake receiver are written from the same design, so they could agree and both
be wrong. This script puts pyatv itself on the other side of each:

  server  pyatv's own receiver half (pyatv/protocols/airplay/server_auth.py, AirPlayServerAuth)
          on 127.0.0.1:0, PIN 1111. Tuner's client pairs (pin-start + pair-setup on one
          connection) and pair-verifies against it; pyatv then reads Tuner's next request with
          the control keys. One JSON line per event on stdout:
            {"event": "ready", "port": P, "pin": "1111", "identifier": "..."}
            {"event": "request", "method": "...", "path": "...", "encrypted": false|true}
            {"event": "verified"}
  client  pyatv's HAP pair-setup and pair-verify (pyatv/protocols/airplay/auth/hap.py) against
          Tuner's fake receiver at 127.0.0.1:--port with --pin, then GET /info over the
          encrypted connection:
            {"event": "paired", "identifier": "..."}
            {"event": "done", "model": "..."}

Driven by the ignored tests at the end of tests/cast_airplay.rs. Run it in a throwaway venv,
isolated (-I), never system-wide, and never against a real TV:

    python -m venv "$VENV" && "$VENV/bin/pip" install pyatv==0.18.0
    TUNER_CAST_PYATV="$VENV/bin/python" \\
        cargo test --locked --test cast_airplay -- --ignored interop

pyatv is MIT-licensed (© 2020 Pierre Ståhl; THIRD_PARTY_NOTICES.md). This script only calls it.
Nothing here leaves 127.0.0.1.
"""

import argparse
import asyncio
import json
import plistlib
import sys

from pyatv.auth.hap_session import HAPSession
from pyatv.auth.hap_srp import SRPAuthHandler
from pyatv.auth.server_auth import PIN_CODE, SERVER_IDENTIFIER
from pyatv.protocols.airplay.auth import verify_connection
from pyatv.protocols.airplay.auth.hap import AirPlayHapPairSetupProcedure
from pyatv.protocols.airplay.server_auth import AirPlayServerAuth
from pyatv.support.http import BasicHttpServer, http_connect, http_server

PYATV_VERSION = "0.18.0"


def emit(event: dict) -> None:
    """One JSON line on stdout, flushed (the Rust test reads them as they come)."""
    print(json.dumps(event), flush=True)


class InteropAuth(AirPlayServerAuth):
    """pyatv's receiver half, reporting each request it routes."""

    def __init__(self, connection: "InteropConnection") -> None:
        super().__init__("pyatv interop", pin=PIN_CODE)
        self.connection = connection

    def handle_request(self, request):
        emit(
            {
                "event": "request",
                "method": request.method,
                "path": request.path,
                "encrypted": self.connection.session is not None,
            }
        )
        return super().handle_request(request)

    def enable_encryption(self, output_key: bytes, input_key: bytes) -> None:
        # pyatv asks for encryption while building M4; the sender reads M4 in plaintext, so the
        # keys take effect once M4 is written (see InteropConnection.process_sent).
        self.connection.pending = (output_key, input_key)


class InteropConnection(BasicHttpServer):
    """One sender connection: plaintext until pair-verify, then HAP frames (HAPSession)."""

    def __init__(self) -> None:
        self.session = None
        self.pending = None
        super().__init__(InteropAuth(self))

    def process_received(self, data: bytes) -> bytes:
        return self.session.decrypt(data) if self.session else data

    def process_sent(self, data: bytes) -> bytes:
        out = self.session.encrypt(data) if self.session else data
        if self.pending:
            session = HAPSession()
            session.enable(*self.pending)
            self.session, self.pending = session, None
            emit({"event": "verified"})
        return out


async def run_server() -> None:
    server, port = await http_server(InteropConnection, address="127.0.0.1")
    emit({"event": "ready", "port": port, "pin": str(PIN_CODE), "identifier": SERVER_IDENTIFIER})
    try:
        await asyncio.Event().wait()
    finally:
        server.close()


async def run_client(port: int, pin: str) -> None:
    connection = await http_connect("127.0.0.1", port)
    try:
        setup = AirPlayHapPairSetupProcedure(connection, SRPAuthHandler())
        await setup.start_pairing()
        credentials = await setup.finish_pairing("", int(pin), "pyatv interop")
    finally:
        connection.close()
    emit({"event": "paired", "identifier": credentials.atv_id.decode()})

    connection = await http_connect("127.0.0.1", port)
    try:
        await verify_connection(credentials, connection)
        # A CSeq, as a sender's RTSP stack adds (pyatv's own receiver insists on one).
        response = await connection.send_and_receive("GET", "/info", headers={"CSeq": "1"})
        info = plistlib.loads(response.body)
    finally:
        connection.close()
    emit({"event": "done", "model": info.get("model")})


def main() -> int:
    import pyatv.const

    if pyatv.const.__version__ != PYATV_VERSION:
        print(f"pyatv {PYATV_VERSION} expected, found {pyatv.const.__version__}", file=sys.stderr)
        return 2
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    modes = parser.add_subparsers(dest="mode", required=True)
    modes.add_parser("server", help="pyatv's receiver for Tuner's client")
    client = modes.add_parser("client", help="pyatv's client against Tuner's fake receiver")
    client.add_argument("--port", type=int, required=True)
    client.add_argument("--pin", default="1111")
    args = parser.parse_args()
    if args.mode == "server":
        asyncio.run(run_server())
    else:
        asyncio.run(run_client(args.port, args.pin))
    return 0


if __name__ == "__main__":
    sys.exit(main())
