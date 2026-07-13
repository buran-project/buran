/*
 * The "buran" SAPI: bridges PHP's SAPI callbacks to Rust functions.
 *
 * Behavioral reference is php-fpm (what frameworks are tested against);
 * implementation follows the Buran spec, section 2.5.
 *
 * Rust side (worker.rs) owns the per-request context; every buran_cb_*
 * symbol below is implemented in Rust and is only called between
 * bphp_sapi_request() enter and leave, on the single worker thread.
 */

#include <main/php.h>
#include <main/SAPI.h>
#include <main/php_main.h>
#include <main/php_variables.h>
#include <zend_exceptions.h>

/* --- Rust callbacks ----------------------------------------------------- */

extern size_t buran_cb_ub_write(const char *str, size_t len);
extern void   buran_cb_headers_begin(int status);
extern void   buran_cb_header_line(const char *line, size_t len);
extern void   buran_cb_headers_end(void);
extern size_t buran_cb_read_post(char *buffer, size_t count);
extern const char *buran_cb_cookies(void); /* NUL-terminated or NULL */
extern void   buran_cb_register_vars(void *track_vars_array);
extern void   buran_cb_log(const char *message);
extern void   buran_cb_finish_request(void);

/* Called back from Rust inside buran_cb_register_vars(). */
void bphp_register_var(void *track_vars_array, const char *name,
                       const char *value, size_t value_len)
{
    /* Takes char* before 8.0 (gained const in 8.0); cast to char* works on
       both. Safe: PHP may rewrite the name buffer in place for array-style
       keys, but our $_SERVER keys have no brackets and the arena is writable. */
    php_register_variable_safe((char *)name, (char *)value,
                               value_len, (zval *)track_vars_array);
}

/* --- SAPI callbacks ------------------------------------------------------ */

static zend_module_entry buran_module_entry;

static int buran_sapi_startup(sapi_module_struct *sapi_module)
{
#if PHP_VERSION_ID < 80200
    /* the trailing arg (number of additional modules) was dropped in 8.2 */
    return php_module_startup(sapi_module, &buran_module_entry, 1);
#else
    return php_module_startup(sapi_module, &buran_module_entry);
#endif
}

static size_t buran_ub_write(const char *str, size_t str_length)
{
    return buran_cb_ub_write(str, str_length);
}

static void buran_flush(void *server_context)
{
    (void)server_context;
}

static int buran_send_headers(sapi_headers_struct *sapi_headers)
{
    zend_llist_position pos;
    sapi_header_struct *h;

    buran_cb_headers_begin(SG(sapi_headers).http_response_code);

    h = zend_llist_get_first_ex(&sapi_headers->headers, &pos);
    while (h != NULL) {
        buran_cb_header_line(h->header, h->header_len);
        h = zend_llist_get_next_ex(&sapi_headers->headers, &pos);
    }

    buran_cb_headers_end();
    return SAPI_HEADER_SENT_SUCCESSFULLY;
}

static size_t buran_read_post(char *buffer, size_t count_bytes)
{
    return buran_cb_read_post(buffer, count_bytes);
}

static char *buran_read_cookies(void)
{
    return (char *)buran_cb_cookies();
}

static void buran_register_variables(zval *track_vars_array)
{
    buran_cb_register_vars((void *)track_vars_array);
}

#if PHP_VERSION_ID < 80000
/* the message parameter gained const in 8.0 */
static void buran_log_message(char *message, int syslog_type_int)
#else
static void buran_log_message(const char *message, int syslog_type_int)
#endif
{
    (void)syslog_type_int;
    buran_cb_log(message);
}

/* --- fastcgi_finish_request() -------------------------------------------
 * FPM-compatible: flush buffers and headers, release the client, keep the
 * script running. Output after the call is swallowed (Rust side guards). */

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_buran_finish_request, 0, 0, _IS_BOOL, 0)
ZEND_END_ARG_INFO()

PHP_FUNCTION(fastcgi_finish_request)
{
    ZEND_PARSE_PARAMETERS_NONE();

    php_output_end_all();
    if (!SG(headers_sent)) {
        sapi_send_headers();
    }
    buran_cb_finish_request();

    RETURN_TRUE;
}

static const zend_function_entry buran_functions[] = {
    PHP_FE(fastcgi_finish_request, arginfo_buran_finish_request)
    /* native-name alias of the same function */
    PHP_FALIAS(buran_finish_request, fastcgi_finish_request, arginfo_buran_finish_request)
    PHP_FE_END
};

static zend_module_entry buran_module_entry = {
    STANDARD_MODULE_HEADER,
    "buran",
    buran_functions,
    NULL, NULL, NULL, NULL, NULL,
    "0.1",
    STANDARD_MODULE_PROPERTIES
};

static sapi_module_struct buran_sapi_module = {
    /* opcache only enables itself for SAPI names on its hardcoded
     * whitelist; an unknown name would silently disable it. */
    .name = "cli-server",
    .pretty_name = "Buran Application Server",
    .startup = buran_sapi_startup,
    .shutdown = php_module_shutdown_wrapper,
    .ub_write = buran_ub_write,
    .flush = buran_flush,
    .send_headers = buran_send_headers,
    .read_post = buran_read_post,
    .read_cookies = buran_read_cookies,
    .register_server_variables = buran_register_variables,
    .log_message = buran_log_message,
};

/* --- Lifecycle ----------------------------------------------------------- */

/* Once per process (in the prototype, before fork).
 * `ini_entries` is an ini-file-shaped string ("key=value\n" lines) applied
 * before module startup — the only way to load zend_extensions (opcache)
 * and set PHP_INI_SYSTEM values. */
int bphp_sapi_boot(const char *ini_path_override, const char *ini_entries)
{
    sapi_startup(&buran_sapi_module);

    if (ini_path_override != NULL) {
        buran_sapi_module.php_ini_path_override = (char *)ini_path_override;
    }
    if (ini_entries != NULL) {
        buran_sapi_module.ini_entries = (char *)ini_entries;
    }

    if (buran_sapi_module.startup(&buran_sapi_module) == FAILURE) {
        return -1;
    }
    return 0;
}

void bphp_sapi_shutdown(void)
{
    php_module_shutdown();
    sapi_shutdown();
}

/*
 * One full request: startup -> execute -> shutdown. All pointer arguments
 * must stay valid for the whole call (owned by the Rust context).
 * The script is passed by filename on purpose: with opcache a cache hit
 * never opens the file at all (fd handoff measurably regressed this).
 * Returns the HTTP response status, or a negative value on engine failure.
 */
int bphp_sapi_request(const char *filename,
                      const char *method,
                      const char *request_uri,
                      const char *query_string,
                      const char *content_type,
                      long content_length,
                      const char *auth_header)
{
    /* volatile: modified across the setjmp/longjmp boundary of zend_first_try.
       Unconditionally rewritten on both exits here, so the standard doesn't
       strictly require it, but kept for parity with bphp_exec in embed_shim.c. */
    volatile int status = -1;

    SG(server_context) = (void *)1; /* non-NULL: request is active */
    SG(request_info).request_method = method;
    SG(request_info).request_uri = (char *)request_uri;
    SG(request_info).query_string = (char *)query_string;
    SG(request_info).content_type = content_type;
    SG(request_info).content_length = content_length;
    SG(request_info).proto_num = 1001;
    SG(request_info).path_translated = NULL;
    SG(sapi_headers).http_response_code = 200;

    if (php_request_startup() == FAILURE) {
        SG(server_context) = NULL;
        return -2;
    }

    if (auth_header != NULL) {
        php_handle_auth_data(auth_header);
    }

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
        php_execute_script(&file_handle);
#if PHP_VERSION_ID >= 80100
        /* before 8.1 the engine destroys the handle itself; an explicit
         * destroy on top of that is a double free */
        zend_destroy_file_handle(&file_handle);
#endif

        status = SG(sapi_headers).http_response_code;
    } zend_catch {
        status = SG(sapi_headers).http_response_code;
        if (status == 200) {
            status = 500;
        }
    } zend_end_try();

    /* Consume nothing further: unread body must not leak to the next
     * request (shared-nothing boundary). */
    SG(post_read) = 1;

    php_request_shutdown(NULL);
    SG(server_context) = NULL;

    return status;
}
