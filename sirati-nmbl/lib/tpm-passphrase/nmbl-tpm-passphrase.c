/*
 * nmbl-tpm-passphrase <luks-device>
 *
 * Unseal the LUKS2 `systemd-tpm2` token of <luks-device> through systemd's
 * token plugin and write the resulting keyslot passphrase to stdout (no
 * trailing newline). Exit 0 only when the passphrase was verified to open a
 * keyslot of that header.
 *
 * NMBL runs this right after `cryptsetup open --token-only` succeeded, while
 * PCR 11 still holds the unseal-time value, and injects the output into the
 * kexec'd initrd as the stage-1 keyfile (`passToStage1`). A kexec drops the
 * dm-crypt mapping NMBL opened, so the next NixOS stage 1 must open the
 * volume again; by then NMBL has extended its handoff into PCR 11 and the
 * TPM would refuse a second unseal. This is the TPM analogue of the
 * passphrase hand-off `luks-password` does.
 *
 * libcryptsetup exposes no public call that returns a token's passphrase, so
 * the plugin's `cryptsetup_token_open` (the stable CRYPTSETUP_TOKEN_1.0 ABI)
 * is called directly on a loaded header.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <libcryptsetup.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#ifndef NMBL_TPM2_TOKEN_PLUGIN
#error "NMBL_TPM2_TOKEN_PLUGIN must name the absolute plugin path"
#endif

typedef int (*token_open_fn)(struct crypt_device *, int, char **, size_t *, void *);
typedef void (*token_free_fn)(void *, size_t);

static int write_all(const char *buf, size_t len)
{
	while (len > 0) {
		ssize_t n = write(STDOUT_FILENO, buf, len);
		if (n <= 0)
			return -1;
		buf += n;
		len -= (size_t)n;
	}
	return 0;
}

int main(int argc, char **argv)
{
	struct crypt_device *cd = NULL;
	void *plugin = NULL;
	token_open_fn token_open;
	token_free_fn token_free;
	char *pass = NULL;
	size_t pass_len = 0;
	int token, max, keyslot, verified, r, rc = 1;

	if (argc != 2) {
		fprintf(stderr, "usage: nmbl-tpm-passphrase <luks-device>\n");
		return 2;
	}

	if (crypt_init(&cd, argv[1]) < 0 || crypt_load(cd, CRYPT_LUKS2, NULL) < 0) {
		fprintf(stderr, "nmbl-tpm-passphrase: %s is not a readable LUKS2 device\n", argv[1]);
		goto out;
	}

	plugin = dlopen(NMBL_TPM2_TOKEN_PLUGIN, RTLD_NOW | RTLD_LOCAL);
	if (!plugin) {
		fprintf(stderr, "nmbl-tpm-passphrase: %s\n", dlerror());
		goto out;
	}
	token_open = (token_open_fn)dlvsym(plugin, "cryptsetup_token_open", "CRYPTSETUP_TOKEN_1.0");
	token_free = (token_free_fn)dlvsym(plugin, "cryptsetup_token_buffer_free", "CRYPTSETUP_TOKEN_1.0");
	if (!token_open) {
		fprintf(stderr, "nmbl-tpm-passphrase: plugin lacks cryptsetup_token_open\n");
		goto out;
	}

	max = crypt_token_max(CRYPT_LUKS2);
	for (token = 0; token < max; token++) {
		const char *type = NULL;
		crypt_token_info info = crypt_token_status(cd, token, &type);

		if (info == CRYPT_TOKEN_INACTIVE || info == CRYPT_TOKEN_INVALID)
			continue;
		if (!type || strcmp(type, "systemd-tpm2") != 0)
			continue;

		r = token_open(cd, token, &pass, &pass_len, NULL);
		if (r < 0 || !pass) {
			pass = NULL;
			continue;
		}
		/*
		 * Verify the passphrase against the token's OWN keyslots (name NULL:
		 * check only, activate nothing). CRYPT_ANY_SLOT would also run the
		 * passphrase keyslot's argon2id (threads, ~1 GiB) for nothing.
		 */
		verified = 0;
		for (keyslot = 0; keyslot < crypt_keyslot_max(CRYPT_LUKS2); keyslot++) {
			if (crypt_token_is_assigned(cd, token, keyslot) != 0)
				continue;
			if (crypt_activate_by_passphrase(cd, NULL, keyslot, pass, pass_len, 0) >= 0) {
				verified = 1;
				break;
			}
		}
		if (!verified)
			fprintf(stderr, "nmbl-tpm-passphrase: token %d passphrase opens none of its keyslots\n", token);
		else if (write_all(pass, pass_len) == 0)
			rc = 0;
		if (token_free)
			token_free(pass, pass_len);
		else
			explicit_bzero(pass, pass_len);
		pass = NULL;
		if (rc == 0)
			break;
	}
	if (rc != 0)
		fprintf(stderr, "nmbl-tpm-passphrase: no systemd-tpm2 token of %s unsealed\n", argv[1]);
out:
	if (plugin)
		dlclose(plugin);
	crypt_free(cd);
	return rc;
}
