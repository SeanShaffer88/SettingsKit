/* SettingsKit.dll — atomic settings files with schema versioning
 * and migrations.
 *
 * A settings file is a JSON object. Keys are addressed by dotted
 * paths ("downloads.preset"). Three rules do the heavy lifting:
 *
 *  1. Defaults are never written. A key reaches the file only when
 *     you sk_set_* it. Reading with a default never creates the key,
 *     so changing a default in your next release cannot silently
 *     change the meaning of files already on disk.
 *  2. The file carries a schema version in the root key "$schema".
 *     Opening an older file runs your registered migration for
 *     each step (1->2, 2->3, ...), in order. Opening a file newer
 *     than your build fails with SK_ERR_SCHEMA_NEWER. A missing
 *     step fails with SK_ERR_NO_MIGRATION.
 *  3. Saves are atomic: temp file in the same directory, flushed,
 *     then a single replace. A crash mid-save leaves the old file
 *     or the new one, never a torn half-file. With SK_SAVE_BACKUP
 *     the previous file is kept as "<name>.bak".
 *
 * A corrupt file fails to open with SK_ERR_PARSE and is left
 * byte-for-byte untouched; starting fresh is your decision, never
 * the library's.
 *
 * Strings are UTF-8, NUL-terminated. char* results are caller-
 * owned: free with sk_free_string(). Failures set a thread-local
 * sk_last_error() (message + code). Use a handle from one thread
 * at a time.
 */
#ifndef SETTINGSKIT_H
#define SETTINGSKIT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(SK_BUILD)
#define SK_API __declspec(dllexport)
#elif defined(_WIN32)
#define SK_API __declspec(dllimport)
#else
#define SK_API
#endif

enum {
    SK_OK = 0,
    SK_ERR_INVALID_ARG = 1,
    SK_ERR_NOT_FOUND = 2,
    SK_ERR_TYPE = 3,        /* key exists but holds a different JSON type */
    SK_ERR_PARSE = 4,       /* file (or JSON value) is not valid */
    SK_ERR_IO = 5,
    SK_ERR_SCHEMA_NEWER = 6,
    SK_ERR_NO_MIGRATION = 7,
    SK_ERR_INTERNAL = 8
};

/* sk_open/sk_create flag: keep "<name>.bak" of the previous file
 * on each save. */
#define SK_SAVE_BACKUP 0x1

typedef struct SkDoc SkDoc;

/* Migration step: upgrade the document from schema `from_version`
 * to `from_version + 1`, using the sk_* API on the doc passed in.
 * Return SK_OK or an SK_ERR_* code. Steps should be re-runnable:
 * if a later step fails, the next open starts the chain again. */
typedef int (*SkMigrateFn)(SkDoc *doc, void *user);

typedef struct {
    int64_t from_version;
    SkMigrateFn migrate;
    void *user;
} SkMigration;

SK_API const char *sk_version(void);
SK_API const char *sk_last_error(void);
SK_API int sk_last_error_code(void);
SK_API void sk_free_string(char *p);

/* Open the settings file at `path` (created on first save when it
 * does not exist yet) for app schema `current_schema`, running
 * migrations as needed. On success *out owns the handle; close it
 * with sk_close(). */
SK_API int sk_open(const char *path, int64_t current_schema,
                   uint32_t flags, SkDoc **out);
SK_API int sk_open_ex(const char *path, int64_t current_schema,
                      uint32_t flags, const SkMigration *migrations,
                      size_t migration_count, SkDoc **out);
/* A document with no file yet; give it one with sk_save_as(). */
SK_API int sk_create(int64_t current_schema, uint32_t flags,
                     SkDoc **out);
/* Frees the handle. Unsaved changes are discarded. */
SK_API void sk_close(SkDoc *doc);

/* Atomic save to the opened path. sk_save_as also re-points the
 * handle at the new path. */
SK_API int sk_save(SkDoc *doc);
SK_API int sk_save_as(SkDoc *doc, const char *path);
/* Discard in-memory changes and re-read the file (migrations
 * re-run). A failed reload leaves the live document unchanged. */
SK_API int sk_reload(SkDoc *doc);

SK_API int sk_schema_version(SkDoc *doc, int64_t *out);
/* 1 = unsaved changes, 0 = clean, -1 = error. */
SK_API int sk_is_dirty(SkDoc *doc);
/* 1 = key present, 0 = absent, -1 = error. */
SK_API int sk_has(SkDoc *doc, const char *path);
/* The whole document as pretty JSON. Caller-owned. */
SK_API char *sk_to_json(SkDoc *doc);

/* Typed getters: absent key -> the default, *found (if non-NULL)
 * set to 1 (present) / 0 (absent) / -1 (error; see sk_last_error).
 * A present key of the wrong JSON type fails with SK_ERR_TYPE. */
SK_API int sk_get_i64(SkDoc *doc, const char *path,
                      int64_t default_value, int64_t *out, int *found);
SK_API int sk_get_f64(SkDoc *doc, const char *path,
                      double default_value, double *out, int *found);
SK_API int sk_get_bool(SkDoc *doc, const char *path,
                       int default_value, int *out, int *found);
/* Caller-owned copy of the value (or of the default; a NULL
 * default with an absent key returns NULL with *found == 0). */
SK_API char *sk_get_string(SkDoc *doc, const char *path,
                           const char *default_value, int *found);
/* The value's compact JSON — the way to read arrays/objects.
 * NULL with *found == 0 means absent; NULL with *found == -1
 * means error. */
SK_API char *sk_get_json(SkDoc *doc, const char *path, int *found);

SK_API int sk_set_i64(SkDoc *doc, const char *path, int64_t value);
SK_API int sk_set_f64(SkDoc *doc, const char *path, double value);
SK_API int sk_set_bool(SkDoc *doc, const char *path, int value);
SK_API int sk_set_string(SkDoc *doc, const char *path,
                         const char *value);
/* Set from JSON text — the way to write arrays/objects. */
SK_API int sk_set_json(SkDoc *doc, const char *path,
                       const char *json_text);
/* 1 = removed, 0 = was absent, -1 = error. */
SK_API int sk_remove(SkDoc *doc, const char *path);

#ifdef __cplusplus
}
#endif
#endif /* SETTINGSKIT_H */
