"""One request to a `morpholog serve` socket, as whichever OS user runs this.

Prints the response line; READY and the ready line when no request is given;
CLOSED when the service hung up without answering; NOCONNECT when the socket
could not be reached. Used by serve_acceptance.sh, which runs it as each user.
"""

import socket
import sys


def connect(path: str) -> socket.socket | None:
    sock = socket.socket(socket.AF_UNIX)
    sock.settimeout(10)
    try:
        sock.connect(path)
    except OSError:
        sock.close()
        return None
    return sock


def exchange(sock: socket.socket, request: str | None) -> str:
    with sock, sock.makefile("rwb") as stream:
        ready = stream.readline()
        if not ready:
            return "CLOSED"
        if request is None:
            return "READY " + ready.decode().strip()
        stream.write(request.encode() + b"\n")
        stream.flush()
        line = stream.readline()
        return line.decode().strip() if line else "CLOSED"


def main() -> None:
    path = sys.argv[1]
    request = sys.argv[2] if len(sys.argv) > 2 else None
    sock = connect(path)
    if sock is None:
        print("NOCONNECT")
        return
    try:
        print(exchange(sock, request))
    except OSError:
        print("CLOSED")


if __name__ == "__main__":
    main()
