/*
 * Phase-0 shim over the embed SAPI: boot once, execute scripts in recycled
 * request contexts. The production module replaces the embed SAPI with a
 * custom "buran" sapi_module_struct; this shim only proves the FFI path and
 * measures per-request costs (spec phase 0).
 */

#include <sapi/embed/php_embed.h>

/* Boots the engine and leaves an active request context. */
int bphp_init(void)
{
    return php_embed_init(0, NULL);
}

/* Executes a script inside the current request context.
 * Returns the script exit status, or -1 on bailout (fatal error). */
int bphp_exec(const char *filename)
{
    /* volatile: modified across the setjmp/longjmp boundary of zend_first_try */
    volatile int status = -1;

    zend_first_try {
        zend_file_handle file_handle;

#if PHP_VERSION_ID < 70400
        /* zend_stream_init_filename() appeared in 7.4 */
        memset(&file_handle, 0, sizeof(file_handle));
        file_handle.type = ZEND_HANDLE_FILENAME;
        file_handle.filename = filename;
#else
        zend_stream_init_filename(&file_handle, filename);
#endif

        if (php_execute_script(&file_handle)) {
            status = EG(exit_status);
        }

#if PHP_VERSION_ID >= 80100
        /* before 8.1 the engine destroys the handle itself; an explicit
         * destroy on top of that is a double free */
        zend_destroy_file_handle(&file_handle);
#endif
    } zend_catch {
        status = -1;
    } zend_end_try();

    return status;
}

/* Finishes the current request context and starts a fresh one:
 * the shared-nothing boundary between requests. */
int bphp_request_recycle(void)
{
    php_request_shutdown(NULL);
    return php_request_startup() == SUCCESS ? 0 : -1;
}

void bphp_shutdown(void)
{
    php_embed_shutdown();
}
