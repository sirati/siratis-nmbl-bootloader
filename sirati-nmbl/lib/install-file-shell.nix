{ pkgs }:

''
  install_nmbl_file_if_changed() {
    source_file="$1"
    destination="$2"
    mode="$3"

    if [ -f "$destination" ] \
      && ${pkgs.diffutils}/bin/cmp -s "$source_file" "$destination"; then
      echo "Unchanged: $destination"
      return 0
    fi

    destination_dir=$(${pkgs.coreutils}/bin/dirname "$destination")
    ${pkgs.coreutils}/bin/mkdir -p "$destination_dir"
    temporary="$destination.nmbl-new.$$"
    ${pkgs.coreutils}/bin/install -m "$mode" "$source_file" "$temporary"
    ${pkgs.coreutils}/bin/sync -f "$temporary"
    ${pkgs.coreutils}/bin/mv -f "$temporary" "$destination"
    ${pkgs.coreutils}/bin/sync -f "$destination_dir"
  }
''
