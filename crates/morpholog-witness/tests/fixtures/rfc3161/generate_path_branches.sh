#!/usr/bin/env bash
# Regenerate path_branches.pem: certification paths with more than one
# candidate issuer under one name, in this order:
#
#   0 root            P-256 certification authority, the anchor
#   1 mid_p521        "intermediate", a P-521 key, which this verifier
#                     cannot check signatures with; signed by root
#   2 leaf_a          signed by mid_p521 (ecdsa-with-SHA256, so the curve
#                     is the only primitive missing)
#   3 mid_p256        "intermediate", a P-256 certification authority;
#                     signed by root
#   4 mid_p256_not_ca the same name and key as mid_p256, but not a
#                     certification authority; signed by root
#   5 leaf_c          signed by the P-256 intermediate key
#
# And path_loop.pem: a leaf, then certification authorities that all have
# the name "loop" and are issued by "loop", each with its own P-521 key.
# Every one is a candidate issuer of every other, the shape that makes an
# unbounded path search do exponential work.
#
# Validity is fixed (2026 to 2050), so the tests' attested time stays inside it
# whenever this is rerun. Needs OpenSSL 3.4 or later for -not_before.
set -euo pipefail
dates=(-not_before 20260101000000Z -not_after 20500101000000Z)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
ca_ext="$work/ca.ext"
not_ca_ext="$work/not_ca.ext"
printf 'basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign\n' > "$ca_ext"
printf 'basicConstraints=critical,CA:FALSE\n' > "$not_ca_ext"

openssl ecparam -name prime256v1 -genkey -noout -out "$work/root.key"
openssl req -x509 -new -key "$work/root.key" -sha256 "${dates[@]}" \
    -subj "/CN=Morpholog test path root" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign" \
    -out "$work/root.pem"

issue() { # key subject issuer_pem issuer_key extfile out
    openssl req -new -key "$1" -subj "$2" -out "$work/req.csr"
    openssl x509 -req -in "$work/req.csr" -CA "$3" -CAkey "$4" -CAcreateserial \
        -sha256 "${dates[@]}" -extfile "$5" -out "$6" 2>/dev/null
}
none_ext="$work/none.ext"
: > "$none_ext"

openssl ecparam -name secp521r1 -genkey -noout -out "$work/mid_p521.key"
issue "$work/mid_p521.key" "/CN=Morpholog test intermediate" \
    "$work/root.pem" "$work/root.key" "$ca_ext" "$work/mid_p521.pem"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/leaf_a.key"
issue "$work/leaf_a.key" "/CN=Morpholog test leaf A" \
    "$work/mid_p521.pem" "$work/mid_p521.key" "$none_ext" "$work/leaf_a.pem"

openssl ecparam -name prime256v1 -genkey -noout -out "$work/mid_p256.key"
issue "$work/mid_p256.key" "/CN=Morpholog test intermediate" \
    "$work/root.pem" "$work/root.key" "$ca_ext" "$work/mid_p256.pem"
issue "$work/mid_p256.key" "/CN=Morpholog test intermediate" \
    "$work/root.pem" "$work/root.key" "$not_ca_ext" "$work/mid_p256_not_ca.pem"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/leaf_c.key"
issue "$work/leaf_c.key" "/CN=Morpholog test leaf C" \
    "$work/mid_p256.pem" "$work/mid_p256.key" "$none_ext" "$work/leaf_c.pem"

cat "$work/root.pem" "$work/mid_p521.pem" "$work/leaf_a.pem" \
    "$work/mid_p256.pem" "$work/mid_p256_not_ca.pem" "$work/leaf_c.pem" \
    > path_branches.pem

: > "$work/loop.pem"
for i in 1 2 3 4 5 6 7 8; do
    openssl ecparam -name secp521r1 -genkey -noout -out "$work/loop$i.key"
    openssl req -x509 -new -key "$work/loop$i.key" -sha256 "${dates[@]}" \
        -subj "/CN=loop" -set_serial "$i" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign" \
        -out "$work/loop$i.pem"
    cat "$work/loop$i.pem" >> "$work/loop.pem"
done
openssl ecparam -name prime256v1 -genkey -noout -out "$work/loop_leaf.key"
issue "$work/loop_leaf.key" "/CN=Morpholog test loop leaf" \
    "$work/loop1.pem" "$work/loop1.key" "$none_ext" "$work/loop_leaf.pem"
cat "$work/loop_leaf.pem" "$work/loop.pem" > path_loop.pem
