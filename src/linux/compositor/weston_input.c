#include "input_wire.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/input-event-codes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include <libweston/libweston.h>
#include <wayland-server-core.h>

/* These are exported libweston entry points, but intentionally not part of the stable public
 * header. Veston supports Weston 13-16. libweston refactored input injection in release 16: the
 * notify_* functions now take a prebuilt event whose base carries the seat and timestamp, and the
 * public header gained the weston_*_event_init helpers used to fill them. Weston 13-15 instead pass
 * the seat, timestamp, and parameters directly. The prototypes and the send helpers below are
 * therefore selected at build time by LIBWESTON_MAJOR, which build.rs derives from the installed
 * libweston-N pkg-config module. */
#ifndef LIBWESTON_MAJOR
#error "LIBWESTON_MAJOR must be defined by build.rs (the installed libweston-N major version)"
#endif

void weston_seat_init(struct weston_seat *seat,
		      struct weston_compositor *compositor,
		      const char *seat_name);
void weston_seat_release(struct weston_seat *seat);
int weston_seat_init_pointer(struct weston_seat *seat);
int weston_seat_init_keyboard(struct weston_seat *seat, struct xkb_keymap *keymap);

#if LIBWESTON_MAJOR >= 16
void notify_motion(const struct weston_pointer_motion_event *event);
void notify_button(const struct weston_pointer_button_event *event);
void notify_axis(const struct weston_pointer_axis_event *event);
void notify_key(const struct weston_key_event *event);
#else
void notify_motion_absolute(struct weston_seat *seat, const struct timespec *time,
			    struct weston_coord_global pos);
void notify_button(struct weston_seat *seat, const struct timespec *time,
		   int32_t button, enum wl_pointer_button_state state);
void notify_axis(struct weston_seat *seat, const struct timespec *time,
		 struct weston_pointer_axis_event *event);
void notify_key(struct weston_seat *seat, const struct timespec *time, uint32_t key,
		enum wl_keyboard_key_state state, enum weston_key_state_update update_state);
#endif

struct veston_input {
	struct weston_compositor *compositor;
	struct weston_seat seat;
	struct wl_event_source *source;
	struct wl_listener destroy_listener;
	int fd;
	uint32_t last_sequence;
	bool seat_initialized;
	bool keys[KEY_MAX + 1];
	bool buttons[KEY_MAX + 1];
};

static void
veston_now(struct timespec *time)
{
	clock_gettime(CLOCK_MONOTONIC, time);
}

static void
send_status(struct veston_input *input, uint16_t type, uint32_t value)
{
	uint8_t packet[sizeof(struct veston_input_header) + sizeof(uint32_t)] = { 0 };
	struct veston_input_header *header = (struct veston_input_header *)packet;
	size_t length = sizeof(*header);

	header->magic_be = htonl(VESTON_INPUT_MAGIC);
	header->version_be = htons(VESTON_INPUT_VERSION);
	header->type_be = htons(type);
	header->sequence_be = 0;
	if (type == VESTON_INPUT_ERROR) {
		header->payload_len_be = htonl(sizeof(uint32_t));
		uint32_t encoded = htonl(value);
		memcpy(packet + sizeof(*header), &encoded, sizeof(encoded));
		length += sizeof(encoded);
	}
	(void)send(input->fd, packet, length, MSG_NOSIGNAL | MSG_DONTWAIT);
}

static void
send_key(struct veston_input *input, uint32_t key, bool pressed)
{
	enum wl_keyboard_key_state state = pressed ?
		WL_KEYBOARD_KEY_STATE_PRESSED : WL_KEYBOARD_KEY_STATE_RELEASED;
	struct timespec time;

	veston_now(&time);
#if LIBWESTON_MAJOR >= 16
	struct weston_key_event event;
	weston_key_event_init(&event, &time, &input->seat, key, state, STATE_UPDATE_AUTOMATIC);
	notify_key(&event);
#else
	notify_key(&input->seat, &time, key, state, STATE_UPDATE_AUTOMATIC);
#endif
	input->keys[key] = pressed;
}

static void
send_button(struct veston_input *input, uint32_t button, bool pressed)
{
	enum wl_pointer_button_state state = pressed ?
		WL_POINTER_BUTTON_STATE_PRESSED : WL_POINTER_BUTTON_STATE_RELEASED;
	struct timespec time;

	veston_now(&time);
#if LIBWESTON_MAJOR >= 16
	struct weston_pointer_button_event event;
	weston_pointer_button_event_init(&event, &time, &input->seat, button, state);
	notify_button(&event);
#else
	notify_button(&input->seat, &time, (int32_t)button, state);
#endif
	input->buttons[button] = pressed;
}

static void
release_all(struct veston_input *input)
{
	uint32_t code;

	for (code = 0; code <= KEY_MAX; ++code) {
		if (input->keys[code])
			send_key(input, code, false);
		if (input->buttons[code])
			send_button(input, code, false);
	}
}

static bool
decode_pair(const uint8_t *payload, uint32_t length, uint32_t *first, uint32_t *second)
{
	struct veston_input_pair encoded;

	if (length != sizeof(encoded))
		return false;
	memcpy(&encoded, payload, sizeof(encoded));
	*first = ntohl(encoded.first_be);
	*second = ntohl(encoded.second_be);
	return true;
}

static bool
dispatch_packet(struct veston_input *input, uint16_t type,
		const uint8_t *payload, uint32_t length)
{
	uint32_t first, second;
	struct timespec time;

	switch (type) {
	case VESTON_INPUT_POINTER_ABSOLUTE: {
		struct weston_coord_global absolute;
		if (!decode_pair(payload, length, &first, &second) ||
		    first > INT32_MAX || second > INT32_MAX)
			return false;
		absolute.c = weston_coord((int32_t)first, (int32_t)second);
		veston_now(&time);
#if LIBWESTON_MAJOR >= 16
		struct weston_pointer_motion_event event;
		weston_pointer_motion_event_init(&event, &time, &input->seat,
			WESTON_POINTER_MOTION_ABS, &absolute, NULL, NULL);
		notify_motion(&event);
#else
		notify_motion_absolute(&input->seat, &time, absolute);
#endif
		return true;
	}
	case VESTON_INPUT_POINTER_BUTTON:
		if (!decode_pair(payload, length, &first, &second) ||
		    first > KEY_MAX || first < BTN_MISC || second > 1 ||
		    input->buttons[first] == (second != 0))
			return false;
		send_button(input, first, second != 0);
		return true;
	case VESTON_INPUT_POINTER_AXIS: {
		int32_t value;
		if (!decode_pair(payload, length, &first, &second) || first > 1)
			return false;
		memcpy(&value, &second, sizeof(value));
		if (value < -12000 || value > 12000)
			return false;
		veston_now(&time);
#if LIBWESTON_MAJOR >= 16
		struct weston_pointer_axis_event event;
		weston_pointer_axis_event_init(&event, &time, &input->seat, first,
			-(double)value / 12.0, true, -value / 120);
		notify_axis(&event);
#else
		struct weston_pointer_axis_event event = {
			.axis = first,
			.value = -(double)value / 12.0,
			.has_discrete = true,
			.discrete = -value / 120,
		};
		notify_axis(&input->seat, &time, &event);
#endif
		return true;
	}
	case VESTON_INPUT_KEY:
		if (!decode_pair(payload, length, &first, &second) ||
		    first == 0 || first > KEY_MAX || second > 1 ||
		    input->keys[first] == (second != 0))
			return false;
		send_key(input, first, second != 0);
		return true;
	case VESTON_INPUT_RELEASE_ALL:
		if (length != 0)
			return false;
		release_all(input);
		return true;
	case VESTON_INPUT_SHUTDOWN:
		if (length != 0)
			return false;
		release_all(input);
		return false;
	default:
		return false;
	}
}

static int
input_ready(int fd, uint32_t mask, void *data)
{
	struct veston_input *input = data;
	uint8_t packet[VESTON_INPUT_MAX_PACKET];
	struct veston_input_header header;
	ssize_t count;
	uint32_t length, sequence;
	uint16_t type;

	if ((mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) != 0)
		goto disconnected;
	if (input->source == NULL || input->fd < 0)
		return 0;
	count = recv(fd, packet, sizeof(packet), MSG_DONTWAIT);
	if (count < 0 && (errno == EAGAIN || errno == EINTR))
		return 0;
	if (count <= 0)
		goto disconnected;
	if ((size_t)count < sizeof(header))
		goto invalid;
	memcpy(&header, packet, sizeof(header));
	length = ntohl(header.payload_len_be);
	sequence = ntohl(header.sequence_be);
	type = ntohs(header.type_be);
	if (ntohl(header.magic_be) != VESTON_INPUT_MAGIC ||
	    ntohs(header.version_be) != VESTON_INPUT_VERSION ||
	    length > VESTON_INPUT_MAX_PACKET - sizeof(header) ||
	    (size_t)count != sizeof(header) + length ||
	    sequence == 0 || sequence != input->last_sequence + 1)
		goto invalid;
	input->last_sequence = sequence;
	if (!dispatch_packet(input, type, packet + sizeof(header), length)) {
		if (type == VESTON_INPUT_SHUTDOWN)
			goto disconnected;
		goto invalid;
	}
	return 0;

invalid:
	send_status(input, VESTON_INPUT_ERROR, EPROTO);

disconnected:
	release_all(input);
	if (input->source) {
		wl_event_source_remove(input->source);
		input->source = NULL;
	}
	if (input->fd >= 0) {
		close(input->fd);
		input->fd = -1;
	}
	return 0;
}

static void
input_destroy(struct wl_listener *listener, void *data)
{
	struct veston_input *input = wl_container_of(listener, input, destroy_listener);
	(void)data;

	if (input->source)
		wl_event_source_remove(input->source);
	release_all(input);
	if (input->seat_initialized)
		weston_seat_release(&input->seat);
	if (input->fd >= 0)
		close(input->fd);
	wl_list_remove(&input->destroy_listener.link);
	free(input);
}

__attribute__((visibility("default"))) int
wet_module_init(struct weston_compositor *compositor, int *argc, char *argv[])
{
	struct veston_input *input;
	struct wl_event_loop *loop;
	const char *value;
	char *end;
	long fd;
	(void)argc;
	(void)argv;

	value = getenv("VESTON_INPUT_FD");
	if (!value)
		return -1;
	errno = 0;
	fd = strtol(value, &end, 10);
	unsetenv("VESTON_INPUT_FD");
	if (errno != 0 || *end != '\0' || fd < 3 || fd > INT32_MAX)
		return -1;
	if (fcntl((int)fd, F_SETFD, FD_CLOEXEC) < 0 ||
	    fcntl((int)fd, F_SETFL, O_NONBLOCK) < 0)
		return -1;

	input = calloc(1, sizeof(*input));
	if (!input)
		return -1;
	input->compositor = compositor;
	input->fd = (int)fd;
	weston_seat_init(&input->seat, compositor, "veston-seat");
	input->seat_initialized = true;
	if (weston_seat_init_pointer(&input->seat) < 0 ||
	    weston_seat_init_keyboard(&input->seat, NULL) < 0)
		goto fail;

	loop = wl_display_get_event_loop(compositor->wl_display);
	input->source = wl_event_loop_add_fd(loop, input->fd,
		WL_EVENT_READABLE | WL_EVENT_HANGUP | WL_EVENT_ERROR,
		input_ready, input);
	if (!input->source)
		goto fail;
	input->destroy_listener.notify = input_destroy;
	wl_signal_add(&compositor->destroy_signal, &input->destroy_listener);
	send_status(input, VESTON_INPUT_READY, 0);
	return 0;

fail:
	if (input->seat_initialized)
		weston_seat_release(&input->seat);
	close(input->fd);
	free(input);
	return -1;
}
