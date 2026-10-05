/* Test-only source metadata translation, used only by mkfs.btrfs --rootdir. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <sys/stat.h>
static int owned(int result, struct stat *metadata) {
    if (result == 0) { metadata->st_uid = 0; metadata->st_gid = 0; }
    return result;
}
int stat(const char *path, struct stat *metadata) {
    int (*original)(const char *, struct stat *) = dlsym(RTLD_NEXT, "stat");
    if (!original) { errno = ENOSYS; return -1; }
    return owned(original(path, metadata), metadata);
}
int lstat(const char *path, struct stat *metadata) {
    int (*original)(const char *, struct stat *) = dlsym(RTLD_NEXT, "lstat");
    if (!original) { errno = ENOSYS; return -1; }
    return owned(original(path, metadata), metadata);
}
int fstat(int descriptor, struct stat *metadata) {
    int (*original)(int, struct stat *) = dlsym(RTLD_NEXT, "fstat");
    if (!original) { errno = ENOSYS; return -1; }
    return owned(original(descriptor, metadata), metadata);
}
/* libc's nftw calls internal stat functions, bypassing symbol interposition. */
#include <ftw.h>
static int (*fixture_callback)(const char *, const struct stat *, int, struct FTW *);
static int owned_callback(const char *path, const struct stat *metadata, int type, struct FTW *walk) {
    struct stat copy = *metadata;
    copy.st_uid = 0; copy.st_gid = 0;
    return fixture_callback(path, &copy, type, walk);
}
int nftw(const char *path, int (*callback)(const char *, const struct stat *, int, struct FTW *), int descriptors, int flags) {
    int (*original)(const char *, int (*)(const char *, const struct stat *, int, struct FTW *), int, int) = dlsym(RTLD_NEXT, "nftw");
    if (!original) { errno = ENOSYS; return -1; }
    fixture_callback = callback;
    return original(path, owned_callback, descriptors, flags);
}
