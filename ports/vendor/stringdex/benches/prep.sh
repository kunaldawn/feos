#!/bin/sh
set -ex
cd `dirname $0`
sha256sum -c wn3.1.dict.tar.gz.SHA256
tar -xvf wn3.1.dict.tar.gz
rm -f words definitions
for x in adj adv noun verb; do
    awk -F' ' '/^ / {next} { for (it = 0; it < $3; ++it) print $1 }' dict/index.$x >> words
    awk -F' ' '/^ / {next} ARGIND == 1 { n=split($0, parts, / \| /); defs[$1] = parts[n] } ARGIND == 2 { for (it = 0; it < $3; ++it) { print defs[$(NF-it)] } }' dict/data.$x dict/index.$x >> definitions
done
head -n 100 < words > words.mini
head -n 100 < definitions > definitions.mini

