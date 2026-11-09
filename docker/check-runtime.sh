#!/bin/sh
# Run at image build time AND in the CI smoke test. Fail closed on tools/packages.
set -eu
for tool in unrar unrar-free rar 7z 7za 7zr 7zz p7zip bsdtar par2 par2create par2repair par2verify ffplay qt-faststart curl; do
    if command -v "$tool" >/dev/null 2>&1; then
        echo "forbidden runtime binary: $tool" >&2; exit 1
    fi
done
if dpkg-query -W -f='${db:Status-Status} ${binary:Package}\n' | \
    grep -E '^installed (libarchive[^ ]*|unrar[^ ]*|rar|p7zip[^ ]*|7zip[^ ]*|par2[^ ]*)(:.*)?$'; then
    echo 'forbidden runtime package' >&2; exit 1
fi
for library in /usr/lib/*/libarchive.so* /usr/lib/libarchive.so* /usr/local/lib/libarchive* /lib/*/libarchive.so*; do
    if [ -e "$library" ]; then
        echo "forbidden runtime library: $library" >&2; exit 1
    fi
done
if ffmpeg -buildconf 2>&1 | grep -q -- '--enable-nonfree'; then
    echo 'nonredistributable FFmpeg build' >&2; exit 1
fi
