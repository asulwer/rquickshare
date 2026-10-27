#!/bin/bash

# Check if the base directory argument is provided
if [ -z "$1" ] || [ -z "$2" ]; then
  echo "Usage: $0 <base_directory> <build_tauri_ver> [debug_mode]"
  exit 1
fi

base_dir="$1"
tauri_ver="$2"
debug_mode="$3"

# Find all relevant files in the base directory
# .msi and .exe (the NSIS -setup.exe installer) are the Windows bundles.
# .app.tar.gz is the macOS app the in-app updater installs, built only when the
# Release workflow asks for updater artifacts.
files=$(find "$base_dir" -type f \( -name "*.deb" -o -name "*.rpm" -o -name "*.AppImage" -o -name "*.dmg" -o -name "*.msi" -o -name "*.exe" -o -name "*.app.tar.gz" \))


# GLIBC only means something on Linux. Git Bash on Windows ships an ldd too,
# which would stamp a meaningless "glibc" version into the Windows filenames.
if [ "$(uname -s)" = "Linux" ] && command -v ldd &> /dev/null; then
    glib_ver=$(ldd --version | head -n1 | awk '{print $NF}')
    echo "GLIBC version: ${glib_ver}"
fi

# Loop through each file
for file in $files; do
    # Extract the directory, filename and extension
    dir=$(dirname "$file")
    filename=$(basename "$file")
    extension="${filename##*.}"

    # Tauri names it after the app alone (rquickshare.app.tar.gz), with no
    # version or architecture to pick out below, so it gets both from here:
    # the version from the Release workflow's VERSION, the architecture from
    # the machine that built it.
    if [[ "$filename" == *.app.tar.gz ]]; then
        if [ -z "$VERSION" ]; then
            echo "VERSION is not set, so $filename can't be named"
            exit 1
        fi
        arch=$(uname -m)
        [ "$arch" = "arm64" ] && arch="aarch64"
        new_filename="r-quick-share-${tauri_ver}_v${VERSION}_${arch}.app.tar.gz"
        mv "$file" "$dir/$new_filename"
        [ -f "$file.sig" ] && mv "$file.sig" "$dir/$new_filename.sig"
        echo "Renamed $filename to $dir/$new_filename"
        continue
    fi

    # Extract the version and anything part. A release's version carries its
    # build number after a + (1.26.298+42), as Cargo only takes three numbers;
    # the file name gets the four-number form, 1.26.298.42.
    ver_re='([0-9]+\.[0-9]+\.[0-9]+(\+[0-9]+)?)'
    if [[ "$filename" =~ ([Rr]-?[Qq]uick[Ss]hare)_?${ver_re}_(.*)\.${extension} ]]; then
        version="${BASH_REMATCH[2]}"
        anything="${BASH_REMATCH[4]}"
    elif [[ "$filename" =~ ([Rr]-?[Qq]uick[Ss]hare)-${ver_re}-([0-9]+)\.(.*)\.${extension} ]]; then
        version="${BASH_REMATCH[2]}"
        anything="${BASH_REMATCH[4]}-${BASH_REMATCH[5]}"
    elif [[ "$filename" =~ ([rR]([-_][qQ]uick[-_][sS]hare))_?${ver_re}_?(.*)\.${extension} ]]; then
        version="${BASH_REMATCH[3]}"
        anything="${BASH_REMATCH[5]}"
    else
        echo "Filename does not match the expected pattern: $filename"
        continue
    fi
    version="v${version/+/.}"

    # Construct the new filename based on the presence of glibc and debug_mode
    if [ -n "$glib_ver" ]; then
        if [ -n "$debug_mode" ]; then
            new_filename="r-quick-share-${tauri_ver}-debug_${version}_glibc-${glib_ver}_${anything}.${extension}"
        else
            new_filename="r-quick-share-${tauri_ver}_${version}_glibc-${glib_ver}_${anything}.${extension}"
        fi
    else
        if [ -n "$debug_mode" ]; then
            new_filename="r-quick-share-${tauri_ver}-debug_${version}_${anything}.${extension}"
        else
            new_filename="r-quick-share-${tauri_ver}_${version}_${anything}.${extension}"
        fi
    fi

    # Rename the file, and the updater signature that goes with it
    mv "$file" "$dir/$new_filename"
    [ -f "$file.sig" ] && mv "$file.sig" "$dir/$new_filename.sig"

    echo "Renamed $filename to $dir/$new_filename"
done

echo "Renaming completed."