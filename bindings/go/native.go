//go:build cgo && (linux || darwin)

package faxe

/*
#cgo pkg-config: faxe
#include <stdlib.h>
#include <faxe.h>
*/
import "C"

import (
	"encoding/json"
	"fmt"
	"unsafe"
)

func decodeBuffer(b C.FaxeBuffer, out any) error {
	defer C.faxe_buffer_free(b)
	data := C.GoBytes(unsafe.Pointer(b.data), C.int(b.len))
	var envelope struct {
		OK     bool            `json:"ok"`
		Result json.RawMessage `json:"result"`
		Error  *Error          `json:"error"`
	}
	if err := json.Unmarshal(data, &envelope); err != nil {
		return fmt.Errorf("faxe: decode native response: %w", err)
	}
	if !envelope.OK {
		if envelope.Error == nil {
			return fmt.Errorf("faxe: malformed native error")
		}
		return envelope.Error
	}
	if out != nil {
		return json.Unmarshal(envelope.Result, out)
	}
	return nil
}
func nativeOpen(config any) (uint64, error) {
	if C.faxe_abi_version() != 1 {
		return 0, fmt.Errorf("faxe: incompatible native ABI")
	}
	data, err := json.Marshal(config)
	if err != nil {
		return 0, err
	}
	p := C.CBytes(data)
	defer C.free(p)
	var result struct {
		Handle uint64 `json:"handle"`
	}
	err = decodeBuffer(C.faxe_open((*C.uint8_t)(p), C.size_t(len(data))), &result)
	return result.Handle, err
}
func nativeCall(handle uint64, request any, out any) error {
	data, err := json.Marshal(request)
	if err != nil {
		return err
	}
	p := C.CBytes(data)
	defer C.free(p)
	return decodeBuffer(C.faxe_call(C.uint64_t(handle), (*C.uint8_t)(p), C.size_t(len(data))), out)
}
func nativePoll(handle uint64, out any) error {
	return decodeBuffer(C.faxe_poll(C.uint64_t(handle), 250), out)
}
func nativeClose(handle uint64) error { return decodeBuffer(C.faxe_close(C.uint64_t(handle)), nil) }
