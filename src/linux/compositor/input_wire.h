#ifndef VESTON_INPUT_WIRE_H
#define VESTON_INPUT_WIRE_H

#include <stdint.h>

#define VESTON_INPUT_MAGIC UINT32_C(0x5653544e)
#define VESTON_INPUT_VERSION UINT16_C(1)
#define VESTON_INPUT_MAX_PACKET UINT32_C(64)

enum veston_input_type {
	VESTON_INPUT_READY = 1,
	VESTON_INPUT_ERROR = 2,
	VESTON_INPUT_POINTER_ABSOLUTE = 3,
	VESTON_INPUT_POINTER_BUTTON = 4,
	VESTON_INPUT_POINTER_AXIS = 5,
	VESTON_INPUT_KEY = 6,
	VESTON_INPUT_RELEASE_ALL = 7,
	VESTON_INPUT_SHUTDOWN = 8,
};

struct veston_input_header {
	uint32_t magic_be;
	uint16_t version_be;
	uint16_t type_be;
	uint32_t payload_len_be;
	uint32_t sequence_be;
} __attribute__((packed));

struct veston_input_pair {
	uint32_t first_be;
	uint32_t second_be;
} __attribute__((packed));

#endif
