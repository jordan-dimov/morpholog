#!/usr/bin/env bash
# Regenerate the ecdsa-with-SHA512 test vectors: for each curve, a throwaway
# certification authority, a timestamping certificate it issues, and one
# token over genesis_payload.bin signed with SHA-512. The chain itself is
# signed with SHA-256, so only the token's signature needs the fallback.
# Run from this directory; it writes ecdsa_sha512_<curve>_{ca.pem,tsr}.
set -euo pipefail
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for curve in prime256v1 secp521r1; do
    openssl ecparam -name prime256v1 -genkey -noout -out "$work/ca.key"
    openssl req -x509 -new -key "$work/ca.key" -sha256 -days 3650 \
        -subj "/CN=Morpholog test CA ($curve)" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign" \
        -out "$work/ca.pem"
    openssl ecparam -name "$curve" -genkey -noout -out "$work/tsa.key"
    openssl req -new -key "$work/tsa.key" -subj "/CN=Morpholog test TSA ($curve)" -out "$work/tsa.csr"
    printf '%s\n' \
        "basicConstraints=critical,CA:FALSE" \
        "keyUsage=critical,digitalSignature" \
        "extendedKeyUsage=critical,timeStamping" > "$work/tsa.ext"
    openssl x509 -req -in "$work/tsa.csr" -CA "$work/ca.pem" -CAkey "$work/ca.key" \
        -CAcreateserial -sha256 -days 3650 -extfile "$work/tsa.ext" -out "$work/tsa.pem"
    cat > "$work/tsa.cnf" <<CNF
[ tsa ]
default_tsa = tsa_config
[ tsa_config ]
serial = $work/serial
signer_cert = $work/tsa.pem
certs = $work/ca.pem
signer_key = $work/tsa.key
signer_digest = sha512
default_policy = 1.2.3.4.1
digests = sha256
accuracy = secs:1
ordering = no
tsa_name = no
ess_cert_id_chain = no
ess_cert_id_alg = sha256
CNF
    echo 01 > "$work/serial"
    openssl ts -query -data genesis_payload.bin -sha256 -cert -out "$work/req.tsq"
    openssl ts -reply -config "$work/tsa.cnf" -queryfile "$work/req.tsq" -out "ecdsa_sha512_$curve.tsr"
    cp "$work/ca.pem" "ecdsa_sha512_${curve}_ca.pem"
done
