# mkfs.erofs compressor flags shared by the rescue EROFS images (stage 2 and
# the tools image). Measured on the minimal DNS-VPS profile (188 MB tree):
# squashfs zstd-19 57 MB; EROFS lz4hc 64 KiB clusters + tail
# packing/fragments/dedupe 75 MB; EROFS zstd-19 128 KiB clusters 52 MB.
compression:
let
  packing = "-Eztailpacking,fragments,dedupe";
in
{
  lz4hc = [ "-zlz4hc,12" "-C65536" packing ];
  zstd = [ "-zzstd,level=19" "-C131072" packing ];
  none = [ ];
}.${compression} or (throw "unsupported rescue image compression `${compression}`")
