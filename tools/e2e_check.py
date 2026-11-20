"""
End-to-end check of a running homestat server, acting as a HomeKit controller.

Uses independent implementations for every protocol layer, so it validates
the Rust code rather than agreeing with itself:
  * aiohomekit (Home Assistant's HomeKit controller) - SRP pair-setup, pair-verify, TLV8
  * cryptography                                    - ChaCha20-Poly1305 session framing
  * pylibsrtp (libsrtp)                             - SRTP/SRTCP decryption + auth
  * PyAV (ffmpeg)                                   - H.264 decoding

Setup:
    uv venv --python 3.12 .venv && uv pip install --python .venv/bin/python aiohomekit pylibsrtp av pillow
Run against a *fresh, unpaired* server (it pairs as a fake controller):
    .venv/bin/python tools/e2e_check.py 031-45-154 [host] [port]
"""

import base64
import json
import os
import socket
import struct
import sys
import time
import uuid

import av
import pylibsrtp
from aiohomekit.protocol import get_session_keys, perform_pair_setup_part1, perform_pair_setup_part2
from aiohomekit.protocol.tlv import TLV
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

PIN = sys.argv[1]
HOST = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 51826
OUT = os.environ.get("E2E_OUT", ".")


def ok(msg):
    print(f"  ✓ {msg}")


class Conn:
    def __init__(self):
        self.sock = socket.create_connection((HOST, PORT), timeout=10)
        self.raw = b""
        self.plain = b""
        self.secure = None

    def enable_encryption(self, derive):
        self.c2a = ChaCha20Poly1305(derive(b"Control-Salt", b"Control-Write-Encryption-Key"))
        self.a2c = ChaCha20Poly1305(derive(b"Control-Salt", b"Control-Read-Encryption-Key"))
        self.out_n = self.in_n = 0
        self.secure = True

    def _send(self, data):
        if not self.secure:
            return self.sock.sendall(data)
        for i in range(0, len(data), 1024):
            chunk = data[i : i + 1024]
            aad = len(chunk).to_bytes(2, "little")
            nonce = b"\0" * 4 + self.out_n.to_bytes(8, "little")
            self.out_n += 1
            self.sock.sendall(aad + self.c2a.encrypt(nonce, chunk, aad))

    def _fill(self):
        data = self.sock.recv(65536)
        if not data:
            raise ConnectionError("closed by accessory")
        if not self.secure:
            self.plain += data
            return
        self.raw += data
        while len(self.raw) >= 2:
            n = int.from_bytes(self.raw[:2], "little")
            if len(self.raw) < 2 + n + 16:
                break
            frame, self.raw = self.raw[: 2 + n + 16], self.raw[2 + n + 16 :]
            nonce = b"\0" * 4 + self.in_n.to_bytes(8, "little")
            self.in_n += 1
            self.plain += self.a2c.decrypt(nonce, frame[2:], frame[:2])

    def request(self, method, path, body=b"", ctype="application/hap+json"):
        head = f"{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nContent-Type: {ctype}\r\nContent-Length: {len(body)}\r\n\r\n"
        self._send(head.encode() + body)
        while b"\r\n\r\n" not in self.plain:
            self._fill()
        head, rest = self.plain.split(b"\r\n\r\n", 1)
        lines = head.decode().split("\r\n")
        status = int(lines[0].split()[1])
        headers = {k.lower(): v.strip() for k, v in (l.split(":", 1) for l in lines[1:])}
        length = int(headers.get("content-length", 0))
        while len(rest) < length:
            self._fill()
            rest = self.plain.split(b"\r\n\r\n", 1)[1]
        self.plain = rest[length:]
        return status, headers, rest[:length]

    def tlv_flow(self, path, gen):
        req = next(gen)
        try:
            while True:
                _, _, body = self.request("POST", path, TLV.encode_list(req[0]), "application/pairing+tlv8")
                req = gen.send(TLV.decode_bytes(body))
        except StopIteration as done:
            return done.value

    def json(self, method, path, obj=None):
        status, _, body = self.request(method, path, json.dumps(obj).encode() if obj is not None else b"")
        return status, (json.loads(body) if body else None)


def tlv(*items):
    return bytes(TLV.encode_list(list(items)))


def b64(b):
    return base64.b64encode(b).decode()


def main():
    print(f"homestat e2e check against {HOST}:{PORT}")

    # --- pair-setup ---------------------------------------------------------
    c = Conn()
    status, _, _ = c.request("GET", "/accessories")
    assert status == 470, status
    ok("unauthenticated /accessories rejected (470)")
    salt, pub = c.tlv_flow("/pair-setup", perform_pair_setup_part1())
    pairing = c.tlv_flow("/pair-setup", perform_pair_setup_part2(PIN, str(uuid.uuid4()).upper(), salt, pub))
    ok(f"pair-setup completed (accessory id {pairing['AccessoryPairingID']})")

    again = Conn()
    _, _, body = again.request("POST", "/pair-setup", tlv((6, b"\x01"), (0, b"\x00")), "application/pairing+tlv8")
    assert dict(TLV.decode_bytes(body)).get(7) == b"\x06", body
    ok("second pair-setup refused with kTLVError_Unavailable")

    # --- pair-verify + encrypted session -----------------------------------
    c = Conn()
    _, derive = c.tlv_flow("/pair-verify", get_session_keys(pairing))
    c.enable_encryption(derive)
    ok("pair-verify completed, session encrypted")

    status, db = c.json("GET", "/accessories")
    assert status == 200
    accs = db["accessories"]
    names = {
        a["aid"]: next(ch["value"] for s in a["services"] for ch in s["characteristics"] if ch["type"] == "23")
        for a in accs
    }
    cams = [a["aid"] for a in accs if any(s["type"] == "110" for s in a["services"])]
    ok(f"/accessories: {len(accs)} accessories {names}; cameras at aids {cams}")
    aid = cams[0]

    status, r = c.json("GET", f"/characteristics?id={aid}.9,{aid}.11,{aid}.13")
    video_cfg = dict(TLV.decode_bytes(base64.b64decode(r["characteristics"][0]["value"])))
    assert 1 in video_cfg
    ok("read supported video/RTP config + streaming status")

    _, _, body = c.request("POST", "/pairings", tlv((6, b"\x01"), (0, b"\x05")), "application/pairing+tlv8")
    ids = [v.decode() for k, v in TLV.decode_bytes(body) if k == 1]
    assert pairing["iOSPairingId"] in ids
    ok(f"list pairings: {len(ids)} controller(s)")

    # --- snapshot -----------------------------------------------------------
    status, headers, jpg = c.request(
        "POST", "/resource", json.dumps({"aid": aid, "resource-type": "image", "image-width": 640, "image-height": 360}).encode()
    )
    assert status == 200 and jpg[:2] == b"\xff\xd8", (status, jpg[:8])
    with open(os.path.join(OUT, "snapshot.jpg"), "wb") as f:
        f.write(jpg)
    ok(f"snapshot: {len(jpg)} byte JPEG")

    # --- live stream --------------------------------------------------------
    udp = socket.socket(socket.AF_INET6 if ":" in HOST else socket.AF_INET, socket.SOCK_DGRAM)
    udp.bind(("::" if ":" in HOST else "0.0.0.0", 0))
    udp.settimeout(0.5)
    vport = udp.getsockname()[1]
    key, msalt = os.urandom(16), os.urandom(14)
    sid = uuid.uuid4().bytes
    addr = tlv((1, b"\x01" if ":" in HOST else b"\x00"), (2, HOST.encode()),
               (3, struct.pack("<H", vport)), (4, struct.pack("<H", vport + 1)))
    srtp = tlv((1, b"\x00"), (2, key), (3, msalt))
    status, _ = c.json("PUT", "/characteristics",
                       {"characteristics": [{"aid": aid, "iid": 14, "value": b64(tlv((1, sid), (3, addr), (4, srtp), (5, srtp)))}]})
    assert status == 204, status
    _, r = c.json("GET", f"/characteristics?id={aid}.14")
    setup = dict(TLV.decode_bytes(base64.b64decode(r["characteristics"][0]["value"])))
    assert setup[1] == sid and setup[2] == b"\x00"
    acc_addr = dict(TLV.decode_bytes(setup[3]))
    ssrc = struct.unpack("<I", setup[6])[0]
    ok(f"setup endpoints: accessory {acc_addr[2].decode()}:{struct.unpack('<H', acc_addr[3])[0]} ssrc {ssrc:#010x}")

    width, height, fps = 640, 360, 30
    video = tlv(
        (1, b"\x00"),
        (2, tlv((1, b"\x00"), (2, b"\x00"), (3, b"\x00"))),
        (3, tlv((1, struct.pack("<H", width)), (2, struct.pack("<H", height)), (3, bytes([fps])))),
        (4, tlv((1, b"\x63"), (2, struct.pack("<I", 0x1234)), (3, struct.pack("<H", 300)), (4, struct.pack("<f", 0.5)), (5, struct.pack("<H", 1378)))),
    )
    start = tlv((1, tlv((1, sid), (2, b"\x01"))), (2, video))
    status, _ = c.json("PUT", "/characteristics", {"characteristics": [{"aid": aid, "iid": 12, "value": b64(start)}]})
    assert status == 204, status
    ok(f"stream started {width}x{height}@{fps}")

    policy = pylibsrtp.Policy(key=key + msalt, ssrc_type=pylibsrtp.Policy.SSRC_ANY_INBOUND)
    session = pylibsrtp.Session(policy)
    rtp_count = rtcp_count = 0
    nals, fu = [], None
    seqs = []
    deadline = time.time() + 4
    while time.time() < deadline:
        try:
            pkt, _ = udp.recvfrom(2048)
        except socket.timeout:
            continue
        if 200 <= pkt[1] <= 204:
            session.unprotect_rtcp(pkt)
            rtcp_count += 1
            continue
        rtp = session.unprotect(pkt)  # raises on bad auth tag
        rtp_count += 1
        assert struct.unpack(">I", rtp[8:12])[0] == ssrc and rtp[1] & 0x7F == 0x63
        seqs.append(struct.unpack(">H", rtp[2:4])[0])
        payload = rtp[12:]
        t = payload[0] & 0x1F
        if t == 28:
            if payload[1] & 0x80:
                fu = bytearray([(payload[0] & 0xE0) | (payload[1] & 0x1F)])
            if fu is not None:
                fu += payload[2:]
                if payload[1] & 0x40:
                    nals.append(bytes(fu))
                    fu = None
        else:
            nals.append(payload)

    stop = tlv((1, tlv((1, sid), (2, b"\x00"))))
    c.json("PUT", "/characteristics", {"characteristics": [{"aid": aid, "iid": 12, "value": b64(stop)}]})
    assert rtp_count > 0, "no RTP received"
    gaps = sum(1 for a, b in zip(seqs, seqs[1:]) if (b - a) & 0xFFFF != 1)
    ok(f"received {rtp_count} SRTP + {rtcp_count} SRTCP packets, all authenticated; {gaps} sequence gaps")

    # The capture window can close mid access unit (e.g. after a keyframe's
    # SPS but before its slices); drop that incomplete tail.
    while nals and nals[-1][0] & 0x1F not in (1, 5):
        nals.pop()
    codec = av.CodecContext.create("h264", "r")
    annexb = b"".join(b"\0\0\0\1" + n for n in nals)
    frames = []
    for packet in codec.parse(annexb) + codec.parse(None):
        frames.extend(codec.decode(packet))
    assert frames, "no frames decoded"
    f = frames[-1]
    f.to_image().save(os.path.join(OUT, "stream_frame.png"))
    types = sorted({n[0] & 0x1F for n in nals})
    ok(f"decoded {len(frames)} H.264 frames ({f.width}x{f.height}), NAL types {types}; profile {codec.profile}")
    print("all checks passed")


if __name__ == "__main__":
    main()
