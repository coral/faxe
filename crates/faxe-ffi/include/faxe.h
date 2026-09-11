#ifndef FAXE_H
#define FAXE_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
/* ABI 1. Inputs are borrowed UTF-8 JSON; Rust retains no input pointers.
 * Every returned buffer is owned by the caller and must be freed exactly once.
 * Responses: {"ok":true,"result":...} or {"ok":false,"error":{"code":...,"message":...}}.
 * Handle IDs are thread-safe. Calls can overlap with poll and close. Close is
 * idempotent, stops owned workers and wakes blocked polls/operations.
 * Only one native SIP engine may run in a process. */
typedef struct FaxeBuffer { uint8_t *data; size_t len; } FaxeBuffer;
uint32_t faxe_abi_version(void);
FaxeBuffer faxe_open(const uint8_t *data, size_t len);
FaxeBuffer faxe_call(uint64_t handle, const uint8_t *data, size_t len);
/* Poll coalesces progress, reconciles durable terminal states, and delivers
 * incoming offers separately. Use one polling consumer. Timeout capped at 1s. */
FaxeBuffer faxe_poll(uint64_t handle, uint32_t timeout_ms);
FaxeBuffer faxe_close(uint64_t handle);
void faxe_buffer_free(FaxeBuffer buffer);
#ifdef __cplusplus
}
#endif
#endif
