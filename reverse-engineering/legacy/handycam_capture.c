#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <getopt.h>
#include <inttypes.h>
#include <libusb.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define SONY_VENDOR_ID 0x054c
#define SONY_PRODUCT_ID 0x00c0
#define VIDEO_INTERFACE 0
#define EP_BOUNDARY 0x81
#define EP_VIDEO 0x82
#define DEFAULT_ALT 5
#define DEFAULT_DURATION 10.0
#define DEFAULT_TRANSFERS 32
#define MAX_TRANSFERS 256

struct app;

struct endpoint_stats {
    uint64_t callbacks;
    uint64_t packets;
    uint64_t nonempty_packets;
    uint64_t packet_errors;
    uint64_t bytes;
};

struct transfer_slot {
    struct app *app;
    struct libusb_transfer *transfer;
    unsigned char *buffer;
    unsigned int id;
    unsigned char endpoint;
    int packet_size;
    bool submitted;
};

struct app {
    libusb_context *usb;
    libusb_device_handle *handle;
    struct transfer_slot *slots;
    size_t slot_count;
    int active_transfers;
    bool running;
    bool stopping;
    int exit_code;
    int alt_setting;
    double duration;
    int transfer_count;
    const char *output_directory;
    const char *init_plan;
    uint64_t init_requests;
    FILE *metadata;
    FILE *ep81;
    FILE *ep82;
    uint64_t start_monotonic_ns;
    uint64_t stop_monotonic_ns;
    uint64_t callback_sequence;
    uint64_t packet_sequence;
    uint64_t ep81_offset;
    uint64_t ep82_offset;
    uint64_t boundary_packets;
    uint64_t video_headers;
    uint64_t candidate_samples;
    bool boundary_pending;
    struct endpoint_stats ep81_stats;
    struct endpoint_stats ep82_stats;
};

static volatile sig_atomic_t interrupted;

static void handle_signal(int signal_number)
{
    (void)signal_number;
    interrupted = 1;
}

static uint64_t monotonic_ns(void)
{
    struct timespec value;

    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        perror("clock_gettime");
        return 0;
    }
    return (uint64_t)value.tv_sec * UINT64_C(1000000000)
        + (uint64_t)value.tv_nsec;
}

static const char *transfer_status_name(enum libusb_transfer_status status)
{
    switch (status) {
    case LIBUSB_TRANSFER_COMPLETED:
        return "completed";
    case LIBUSB_TRANSFER_ERROR:
        return "error";
    case LIBUSB_TRANSFER_TIMED_OUT:
        return "timed_out";
    case LIBUSB_TRANSFER_CANCELLED:
        return "cancelled";
    case LIBUSB_TRANSFER_STALL:
        return "stall";
    case LIBUSB_TRANSFER_NO_DEVICE:
        return "no_device";
    case LIBUSB_TRANSFER_OVERFLOW:
        return "overflow";
    }
    return "unknown";
}

static const char *speed_name(int speed)
{
    switch (speed) {
    case LIBUSB_SPEED_LOW:
        return "low";
    case LIBUSB_SPEED_FULL:
        return "full";
    case LIBUSB_SPEED_HIGH:
        return "high";
    case LIBUSB_SPEED_SUPER:
        return "super";
    case LIBUSB_SPEED_SUPER_PLUS:
        return "super-plus";
    default:
        return "unknown";
    }
}

static int video_packet_size(int alternate_setting)
{
    if (alternate_setting == 7)
        return 1023;
    return 128 + alternate_setting * 128;
}

static struct endpoint_stats *stats_for(
    struct app *app, unsigned char endpoint)
{
    return endpoint == EP_BOUNDARY ? &app->ep81_stats : &app->ep82_stats;
}

static FILE *file_for(struct app *app, unsigned char endpoint)
{
    return endpoint == EP_BOUNDARY ? app->ep81 : app->ep82;
}

static uint64_t *offset_for(struct app *app, unsigned char endpoint)
{
    return endpoint == EP_BOUNDARY ? &app->ep81_offset : &app->ep82_offset;
}

static void mark_stopping(struct app *app, int exit_code)
{
    app->running = false;
    app->stopping = true;
    if (exit_code != 0 && app->exit_code == 0)
        app->exit_code = exit_code;
}

static bool has_sony_header(const unsigned char *data, int length)
{
    int index;

    if (length < 6)
        return false;
    for (index = 0; index < 6; ++index) {
        if (data[index] != 0xff)
            return false;
    }
    return true;
}

static void record_packet(
    struct transfer_slot *slot,
    unsigned int packet_index,
    uint64_t completion_ns,
    uint64_t callback_sequence)
{
    struct app *app = slot->app;
    struct libusb_iso_packet_descriptor *descriptor =
        &slot->transfer->iso_packet_desc[packet_index];
    struct endpoint_stats *stats =
        stats_for(app, slot->endpoint);
    unsigned char *data =
        libusb_get_iso_packet_buffer_simple(slot->transfer, packet_index);
    uint64_t *data_offset = offset_for(app, slot->endpoint);
    int length = descriptor->actual_length;
    bool boundary = false;
    bool header = false;
    int timestamp_ms = -1;

    ++app->packet_sequence;
    ++stats->packets;
    if (descriptor->status != LIBUSB_TRANSFER_COMPLETED) {
        ++stats->packet_errors;
        length = 0;
    }

    if (length > 0) {
        FILE *output = file_for(app, slot->endpoint);

        if (fwrite(data, 1, (size_t)length, output) != (size_t)length) {
            perror("writing endpoint data");
            mark_stopping(app, 1);
            return;
        }
        ++stats->nonempty_packets;
        stats->bytes += (uint64_t)length;

        if (slot->endpoint == EP_BOUNDARY) {
            boundary = (data[0] & 0x08) != 0;
            if (boundary) {
                ++app->boundary_packets;
                app->boundary_pending = true;
            }
        } else {
            header = has_sony_header(data, length);
            if (header) {
                ++app->video_headers;
                if (length >= 8)
                    timestamp_ms = ((data[6] & 0x07) << 8) | data[7];
                if (app->boundary_pending) {
                    ++app->candidate_samples;
                    app->boundary_pending = false;
                }
            }
        }
    }

    fprintf(
        app->metadata,
        "{\"packet_sequence\":%" PRIu64
        ",\"callback_sequence\":%" PRIu64
        ",\"completion_monotonic_ns\":%" PRIu64
        ",\"endpoint\":\"0x%02x\""
        ",\"transfer_id\":%u"
        ",\"packet_index\":%u"
        ",\"status\":\"%s\""
        ",\"requested_length\":%u"
        ",\"length\":%d"
        ",\"data_offset\":",
        app->packet_sequence,
        callback_sequence,
        completion_ns,
        slot->endpoint,
        slot->id,
        packet_index,
        transfer_status_name(descriptor->status),
        descriptor->length,
        length);
    if (length > 0)
        fprintf(app->metadata, "%" PRIu64, *data_offset);
    else
        fputs("null", app->metadata);
    fprintf(
        app->metadata,
        ",\"boundary\":%s,\"header\":%s,\"timestamp_ms\":",
        boundary ? "true" : "false",
        header ? "true" : "false");
    if (timestamp_ms >= 0)
        fprintf(app->metadata, "%d", timestamp_ms);
    else
        fputs("null", app->metadata);
    fputs("}\n", app->metadata);

    if (length > 0)
        *data_offset += (uint64_t)length;
}

static void LIBUSB_CALL transfer_callback(struct libusb_transfer *transfer)
{
    struct transfer_slot *slot = transfer->user_data;
    struct app *app = slot->app;
    struct endpoint_stats *stats =
        stats_for(app, slot->endpoint);
    uint64_t completion_ns = monotonic_ns();
    uint64_t callback_sequence = ++app->callback_sequence;
    int result;
    int packet_index;

    ++stats->callbacks;
    if (transfer->status == LIBUSB_TRANSFER_COMPLETED) {
        for (packet_index = 0;
             packet_index < transfer->num_iso_packets;
             ++packet_index) {
            record_packet(
                slot,
                (unsigned int)packet_index,
                completion_ns,
                callback_sequence);
        }
    } else if (transfer->status != LIBUSB_TRANSFER_CANCELLED) {
        fprintf(
            stderr,
            "endpoint 0x%02x transfer %u: %s\n",
            slot->endpoint,
            slot->id,
            transfer_status_name(transfer->status));
        if (transfer->status == LIBUSB_TRANSFER_NO_DEVICE)
            mark_stopping(app, 1);
    }

    if (app->running) {
        transfer->actual_length = 0;
        result = libusb_submit_transfer(transfer);
        if (result == 0)
            return;
        fprintf(
            stderr,
            "could not resubmit endpoint 0x%02x transfer %u: %s\n",
            slot->endpoint,
            slot->id,
            libusb_error_name(result));
        mark_stopping(app, 1);
    }

    slot->submitted = false;
    --app->active_transfers;
}

static int path_for(
    char *buffer, size_t buffer_size, const char *directory, const char *name)
{
    int length = snprintf(buffer, buffer_size, "%s/%s", directory, name);

    if (length < 0 || (size_t)length >= buffer_size) {
        fprintf(stderr, "output path is too long: %s/%s\n", directory, name);
        return -1;
    }
    return 0;
}

static int open_outputs(struct app *app)
{
    char path[4096];

    if (mkdir(app->output_directory, 0755) != 0) {
        fprintf(
            stderr,
            "could not create output directory %s: %s\n",
            app->output_directory,
            strerror(errno));
        return -1;
    }

#define OPEN_OUTPUT(member, filename, mode)                                      \
    do {                                                                         \
        if (path_for(path, sizeof(path), app->output_directory, filename) != 0)  \
            return -1;                                                           \
        app->member = fopen(path, mode);                                          \
        if (app->member == NULL) {                                                \
            fprintf(stderr, "could not open %s: %s\n", path, strerror(errno));   \
            return -1;                                                           \
        }                                                                        \
    } while (0)

    OPEN_OUTPUT(metadata, "iso-packets.jsonl", "w");
    OPEN_OUTPUT(ep81, "ep81.bin", "wb");
    OPEN_OUTPUT(ep82, "ep82.bin", "wb");
#undef OPEN_OUTPUT
    return 0;
}

static int write_summary(struct app *app, libusb_device *device)
{
    char path[4096];
    FILE *summary;
    uint8_t bus = libusb_get_bus_number(device);
    uint8_t address = libusb_get_device_address(device);
    double elapsed =
        (double)(app->stop_monotonic_ns - app->start_monotonic_ns)
        / 1000000000.0;

    if (path_for(
            path, sizeof(path), app->output_directory, "summary.json")
        != 0)
        return -1;
    summary = fopen(path, "w");
    if (summary == NULL) {
        fprintf(stderr, "could not open %s: %s\n", path, strerror(errno));
        return -1;
    }
    fprintf(
        summary,
        "{\n"
        "  \"device\": {\"vendor_id\": \"0x054c\", "
        "\"product_id\": \"0x00c0\", \"bus\": %u, \"address\": %u},\n"
        "  \"interface\": 0,\n"
        "  \"alternate_setting\": %d,\n"
        "  \"requested_duration_seconds\": %.6f,\n"
        "  \"elapsed_seconds\": %.6f,\n"
        "  \"transfers_per_endpoint\": %d,\n"
        "  \"callback_sequence_count\": %" PRIu64 ",\n"
        "  \"packet_sequence_count\": %" PRIu64 ",\n"
        "  \"boundary_packets\": %" PRIu64 ",\n"
        "  \"video_headers\": %" PRIu64 ",\n"
        "  \"candidate_samples\": %" PRIu64 ",\n"
        "  \"ordering_note\": "
        "\"packet_sequence follows libusb callback order; usbfs frame "
        "numbers are not exposed by libusb\",\n"
        "  \"endpoints\": {\n"
        "    \"0x81\": {\"packet_size\": 8, \"callbacks\": %" PRIu64
        ", \"packets\": %" PRIu64 ", \"nonempty_packets\": %" PRIu64
        ", \"packet_errors\": %" PRIu64 ", \"bytes\": %" PRIu64 "},\n"
        "    \"0x82\": {\"packet_size\": %d, \"callbacks\": %" PRIu64
        ", \"packets\": %" PRIu64 ", \"nonempty_packets\": %" PRIu64
        ", \"packet_errors\": %" PRIu64 ", \"bytes\": %" PRIu64 "}\n"
        "  }\n"
        "}\n",
        bus,
        address,
        app->alt_setting,
        app->duration,
        elapsed,
        app->transfer_count,
        app->callback_sequence,
        app->packet_sequence,
        app->boundary_packets,
        app->video_headers,
        app->candidate_samples,
        app->ep81_stats.callbacks,
        app->ep81_stats.packets,
        app->ep81_stats.nonempty_packets,
        app->ep81_stats.packet_errors,
        app->ep81_stats.bytes,
        video_packet_size(app->alt_setting),
        app->ep82_stats.callbacks,
        app->ep82_stats.packets,
        app->ep82_stats.nonempty_packets,
        app->ep82_stats.packet_errors,
        app->ep82_stats.bytes);
    if (fclose(summary) != 0) {
        perror("closing summary");
        return -1;
    }
    return 0;
}

static int allocate_slots(struct app *app)
{
    size_t slot_count = (size_t)app->transfer_count * 2;
    size_t index;

    app->slots = calloc(slot_count, sizeof(*app->slots));
    if (app->slots == NULL) {
        perror("allocating transfer slots");
        return -1;
    }
    app->slot_count = slot_count;

    for (index = 0; index < slot_count; ++index) {
        struct transfer_slot *slot = &app->slots[index];

        slot->app = app;
        slot->id = (unsigned int)index;
        slot->endpoint =
            index % 2 == 0 ? EP_BOUNDARY : EP_VIDEO;
        slot->packet_size = slot->endpoint == EP_BOUNDARY
            ? 8
            : video_packet_size(app->alt_setting);
        slot->buffer = malloc((size_t)slot->packet_size);
        slot->transfer = libusb_alloc_transfer(1);
        if (slot->buffer == NULL || slot->transfer == NULL) {
            fprintf(stderr, "could not allocate transfer slot %zu\n", index);
            return -1;
        }
        libusb_fill_iso_transfer(
            slot->transfer,
            app->handle,
            slot->endpoint,
            slot->buffer,
            slot->packet_size,
            1,
            transfer_callback,
            slot,
            1000);
        libusb_set_iso_packet_lengths(
            slot->transfer, (unsigned int)slot->packet_size);
    }
    return 0;
}

static int submit_slots(struct app *app)
{
    size_t index;

    for (index = 0; index < app->slot_count; ++index) {
        int result = libusb_submit_transfer(app->slots[index].transfer);

        if (result != 0) {
            fprintf(
                stderr,
                "could not submit endpoint 0x%02x transfer %u: %s\n",
                app->slots[index].endpoint,
                app->slots[index].id,
                libusb_error_name(result));
            mark_stopping(app, 1);
            return -1;
        }
        app->slots[index].submitted = true;
        ++app->active_transfers;
    }
    return 0;
}

static void cancel_slots(struct app *app)
{
    size_t index;

    if (!app->stopping)
        app->stopping = true;
    app->running = false;
    for (index = 0; index < app->slot_count; ++index) {
        if (app->slots[index].submitted) {
            int result =
                libusb_cancel_transfer(app->slots[index].transfer);
            if (result != 0 && result != LIBUSB_ERROR_NOT_FOUND) {
                fprintf(
                    stderr,
                    "could not cancel transfer %u: %s\n",
                    app->slots[index].id,
                    libusb_error_name(result));
            }
        }
    }
}

static void free_slots(struct app *app)
{
    size_t index;

    for (index = 0; index < app->slot_count; ++index) {
        libusb_free_transfer(app->slots[index].transfer);
        free(app->slots[index].buffer);
    }
    free(app->slots);
}

static void close_outputs(struct app *app)
{
    if (app->metadata != NULL)
        fclose(app->metadata);
    if (app->ep81 != NULL)
        fclose(app->ep81);
    if (app->ep82 != NULL)
        fclose(app->ep82);
}

static int parse_integer(
    const char *text, int minimum, int maximum, const char *name)
{
    char *end;
    long value;

    errno = 0;
    value = strtol(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0'
        || value < minimum || value > maximum) {
        fprintf(
            stderr,
            "%s must be between %d and %d\n",
            name,
            minimum,
            maximum);
        return -1;
    }
    return (int)value;
}

static int parse_duration(const char *text, double *duration)
{
    char *end;
    double value;

    errno = 0;
    value = strtod(text, &end);
    if (errno != 0 || end == text || *end != '\0' || value <= 0.0) {
        fputs("duration must be greater than zero\n", stderr);
        return -1;
    }
    *duration = value;
    return 0;
}

static int sleep_microseconds(unsigned long delay_us)
{
    struct timespec delay = {
        .tv_sec = (time_t)(delay_us / 1000000),
        .tv_nsec = (long)(delay_us % 1000000) * 1000,
    };

    while (nanosleep(&delay, &delay) != 0) {
        if (errno != EINTR) {
            perror("nanosleep");
            return -1;
        }
        if (interrupted)
            return -1;
    }
    return 0;
}

static int parse_unsigned(
    const char *text, unsigned long maximum, unsigned long *output)
{
    char *end;
    unsigned long value;

    errno = 0;
    value = strtoul(text, &end, 0);
    if (errno != 0 || end == text || *end != '\0' || value > maximum)
        return -1;
    *output = value;
    return 0;
}

static int hex_nibble(char value)
{
    if (value >= '0' && value <= '9')
        return value - '0';
    if (value >= 'a' && value <= 'f')
        return value - 'a' + 10;
    if (value >= 'A' && value <= 'F')
        return value - 'A' + 10;
    return -1;
}

static int decode_hex(
    const char *text, unsigned char *output, size_t expected_length)
{
    size_t index;

    if (strlen(text) != expected_length * 2)
        return -1;
    for (index = 0; index < expected_length; ++index) {
        int high = hex_nibble(text[index * 2]);
        int low = hex_nibble(text[index * 2 + 1]);

        if (high < 0 || low < 0)
            return -1;
        output[index] = (unsigned char)((high << 4) | low);
    }
    return 0;
}

static int replay_init_plan(struct app *app)
{
    FILE *plan;
    char *line = NULL;
    size_t capacity = 0;
    unsigned long line_number = 0;
    int result = -1;

    plan = fopen(app->init_plan, "r");
    if (plan == NULL) {
        fprintf(
            stderr,
            "could not open initialization plan %s: %s\n",
            app->init_plan,
            strerror(errno));
        return -1;
    }

    while (getline(&line, &capacity, plan) >= 0) {
        char *fields[8];
        char *save = NULL;
        char *token;
        size_t field_count = 0;
        unsigned long delay_us;
        unsigned long request_type;
        unsigned long request;
        unsigned long value;
        unsigned long index;
        unsigned long length;
        unsigned char data[64] = {0};
        int transferred;

        ++line_number;
        if (line[0] == '#' || line[0] == '\n' || line[0] == '\0')
            continue;
        line[strcspn(line, "\r\n")] = '\0';
        for (token = strtok_r(line, "\t", &save);
             token != NULL && field_count < 8;
             token = strtok_r(NULL, "\t", &save)) {
            fields[field_count++] = token;
        }
        if (field_count != 8) {
            fprintf(
                stderr,
                "%s:%lu: expected 8 tab-separated fields, got %zu\n",
                app->init_plan,
                line_number,
                field_count);
            goto done;
        }
        if (parse_unsigned(fields[0], 10000000, &delay_us) != 0
            || parse_unsigned(fields[1], 0xff, &request_type) != 0
            || parse_unsigned(fields[2], 0xff, &request) != 0
            || parse_unsigned(fields[3], 0xffff, &value) != 0
            || parse_unsigned(fields[4], 0xffff, &index) != 0
            || parse_unsigned(fields[5], sizeof(data), &length) != 0) {
            fprintf(
                stderr,
                "%s:%lu: invalid numeric field\n",
                app->init_plan,
                line_number);
            goto done;
        }
        if (sleep_microseconds(delay_us) != 0)
            goto done;

        if (request_type == 0x01 && request == 0x0b
            && index == VIDEO_INTERFACE && length == 0) {
            transferred = libusb_set_interface_alt_setting(
                app->handle, VIDEO_INTERFACE, (int)value);
            if (transferred != 0) {
                fprintf(
                    stderr,
                    "%s:%lu: SET_INTERFACE alt %lu failed: %s\n",
                    app->init_plan,
                    line_number,
                    value,
                    libusb_error_name(transferred));
                goto done;
            }
        } else if (
            (request_type == 0x40 || request_type == 0xc0)
            && request == 0x88) {
            if (request_type == 0x40
                && decode_hex(fields[6], data, length) != 0) {
                fprintf(
                    stderr,
                    "%s:%lu: invalid %lu-byte output payload\n",
                    app->init_plan,
                    line_number,
                    length);
                goto done;
            }
            transferred = libusb_control_transfer(
                app->handle,
                (uint8_t)request_type,
                (uint8_t)request,
                (uint16_t)value,
                (uint16_t)index,
                data,
                (uint16_t)length,
                1000);
            if (transferred < 0) {
                fprintf(
                    stderr,
                    "%s:%lu: request 0x%02lx index 0x%04lx failed: %s\n",
                    app->init_plan,
                    line_number,
                    request,
                    index,
                    libusb_error_name(transferred));
                goto done;
            }
            if ((unsigned long)transferred != length) {
                fprintf(
                    stderr,
                    "%s:%lu: request 0x%02lx index 0x%04lx returned "
                    "%d of %lu bytes\n",
                    app->init_plan,
                    line_number,
                    request,
                    index,
                    transferred,
                    length);
                goto done;
            }
        } else {
            fprintf(
                stderr,
                "%s:%lu: refusing unsupported request "
                "type=0x%02lx request=0x%02lx index=0x%04lx\n",
                app->init_plan,
                line_number,
                request_type,
                request,
                index);
            goto done;
        }
        ++app->init_requests;
    }
    if (ferror(plan)) {
        fprintf(
            stderr,
            "error reading initialization plan %s\n",
            app->init_plan);
        goto done;
    }
    result = 0;

done:
    free(line);
    fclose(plan);
    return result;
}

static void usage(FILE *stream, const char *program)
{
    fprintf(
        stream,
        "Usage: %s [OPTIONS] OUTPUT_DIRECTORY\n"
        "\n"
        "Capture raw Sony DCR-HC24 isochronous video transport data.\n"
        "\n"
        "Options:\n"
        "  -d, --duration SECONDS   capture duration (default %.0f)\n"
        "  -a, --alt N              interface-0 alternate setting 1-7 "
        "(default %d)\n"
        "  -t, --transfers N        one-packet transfers per endpoint "
        "(default %d)\n"
        "  -i, --init FILE          replay a traced Sony initialization plan\n"
        "  -h, --help               show this help\n",
        program,
        DEFAULT_DURATION,
        DEFAULT_ALT,
        DEFAULT_TRANSFERS);
}

int main(int argc, char **argv)
{
    static const struct option options[] = {
        {"duration", required_argument, NULL, 'd'},
        {"alt", required_argument, NULL, 'a'},
        {"transfers", required_argument, NULL, 't'},
        {"init", required_argument, NULL, 'i'},
        {"help", no_argument, NULL, 'h'},
        {NULL, 0, NULL, 0},
    };
    struct app app = {
        .running = true,
        .alt_setting = DEFAULT_ALT,
        .duration = DEFAULT_DURATION,
        .transfer_count = DEFAULT_TRANSFERS,
    };
    libusb_device *device = NULL;
    struct sigaction action = {
        .sa_handler = handle_signal,
    };
    int interface_claimed = 0;
    int option;
    int result;

    while ((option = getopt_long(
                argc, argv, "d:a:t:i:h", options, NULL))
           != -1) {
        switch (option) {
        case 'd':
            if (parse_duration(optarg, &app.duration) != 0)
                return 2;
            break;
        case 'a':
            app.alt_setting = parse_integer(
                optarg, 1, 7, "alternate setting");
            if (app.alt_setting < 0)
                return 2;
            break;
        case 't':
            app.transfer_count = parse_integer(
                optarg, 1, MAX_TRANSFERS, "transfer count");
            if (app.transfer_count < 0)
                return 2;
            break;
        case 'i':
            app.init_plan = optarg;
            break;
        case 'h':
            usage(stdout, argv[0]);
            return 0;
        default:
            usage(stderr, argv[0]);
            return 2;
        }
    }
    if (optind + 1 != argc) {
        usage(stderr, argv[0]);
        return 2;
    }
    app.output_directory = argv[optind];

    sigemptyset(&action.sa_mask);
    if (sigaction(SIGINT, &action, NULL) != 0
        || sigaction(SIGTERM, &action, NULL) != 0) {
        perror("sigaction");
        return 1;
    }
    if (open_outputs(&app) != 0) {
        close_outputs(&app);
        return 1;
    }
    result = libusb_init(&app.usb);
    if (result != 0) {
        fprintf(stderr, "libusb_init: %s\n", libusb_error_name(result));
        app.exit_code = 1;
        goto cleanup;
    }
    app.handle = libusb_open_device_with_vid_pid(
        app.usb, SONY_VENDOR_ID, SONY_PRODUCT_ID);
    if (app.handle == NULL) {
        fputs(
            "could not open 054c:00c0; check that the camera is connected "
            "and the device node is writable\n",
            stderr);
        app.exit_code = 1;
        goto cleanup;
    }
    device = libusb_get_device(app.handle);
    fprintf(
        stderr,
        "Opened 054c:00c0 on bus %u address %u at %s speed\n",
        libusb_get_bus_number(device),
        libusb_get_device_address(device),
        speed_name(libusb_get_device_speed(device)));

    result = libusb_kernel_driver_active(app.handle, VIDEO_INTERFACE);
    if (result == 1) {
        fputs(
            "interface 0 has a kernel driver; refusing to detach it "
            "automatically\n",
            stderr);
        app.exit_code = 1;
        goto cleanup;
    }
    if (result < 0 && result != LIBUSB_ERROR_NOT_SUPPORTED) {
        fprintf(
            stderr,
            "could not query interface-0 driver: %s\n",
            libusb_error_name(result));
        app.exit_code = 1;
        goto cleanup;
    }
    result = libusb_claim_interface(app.handle, VIDEO_INTERFACE);
    if (result != 0) {
        fprintf(
            stderr,
            "could not claim interface 0: %s\n",
            libusb_error_name(result));
        app.exit_code = 1;
        goto cleanup;
    }
    interface_claimed = 1;

    if (app.init_plan != NULL) {
        fprintf(
            stderr,
            "Replaying initialization plan %s\n",
            app.init_plan);
        if (replay_init_plan(&app) != 0) {
            app.exit_code = 1;
            goto cleanup;
        }
        fprintf(
            stderr,
            "Replayed %" PRIu64 " initialization requests\n",
            app.init_requests);
    } else {
        result = libusb_set_interface_alt_setting(
            app.handle, VIDEO_INTERFACE, app.alt_setting);
        if (result != 0) {
            fprintf(
                stderr,
                "could not select interface 0 alt %d: %s\n",
                app.alt_setting,
                libusb_error_name(result));
            app.exit_code = 1;
            goto cleanup;
        }
    }
    fprintf(
        stderr,
        "Selected interface 0 alt %d (0x81: 8 bytes, 0x82: %d bytes)\n",
        app.alt_setting,
        video_packet_size(app.alt_setting));

    if (allocate_slots(&app) != 0) {
        app.exit_code = 1;
        goto cleanup;
    }
    app.start_monotonic_ns = monotonic_ns();
    if (submit_slots(&app) != 0)
        goto event_loop;

event_loop:
    while (app.active_transfers > 0) {
        struct timeval timeout = {
            .tv_sec = 0,
            .tv_usec = 100000,
        };
        uint64_t now = monotonic_ns();
        double elapsed =
            (double)(now - app.start_monotonic_ns) / 1000000000.0;

        if (app.running && (interrupted || elapsed >= app.duration)) {
            if (interrupted)
                fputs("Interrupted; stopping transfers\n", stderr);
            cancel_slots(&app);
        } else if (!app.running && !app.stopping) {
            cancel_slots(&app);
        }

        result = libusb_handle_events_timeout_completed(
            app.usb, &timeout, NULL);
        if (result != 0 && result != LIBUSB_ERROR_INTERRUPTED) {
            fprintf(
                stderr,
                "libusb event handling failed: %s\n",
                libusb_error_name(result));
            mark_stopping(&app, 1);
            cancel_slots(&app);
        }
    }
    app.stop_monotonic_ns = monotonic_ns();

cleanup:
    if (app.active_transfers > 0) {
        cancel_slots(&app);
        while (app.active_transfers > 0) {
            struct timeval timeout = {
                .tv_sec = 0,
                .tv_usec = 100000,
            };
            libusb_handle_events_timeout_completed(app.usb, &timeout, NULL);
        }
    }
    if (device != NULL && app.start_monotonic_ns != 0) {
        if (app.stop_monotonic_ns == 0)
            app.stop_monotonic_ns = monotonic_ns();
        if (write_summary(&app, device) != 0)
            app.exit_code = 1;
    }
    free_slots(&app);
    if (interface_claimed) {
        result = libusb_set_interface_alt_setting(
            app.handle, VIDEO_INTERFACE, 0);
        if (result != 0) {
            fprintf(
                stderr,
                "warning: could not restore interface 0 alt 0: %s\n",
                libusb_error_name(result));
        }
    }
    if (interface_claimed)
        libusb_release_interface(app.handle, VIDEO_INTERFACE);
    if (app.handle != NULL)
        libusb_close(app.handle);
    if (app.usb != NULL)
        libusb_exit(app.usb);
    close_outputs(&app);

    if (app.start_monotonic_ns != 0) {
        fprintf(
            stderr,
            "Captured %" PRIu64 " boundary packets, %" PRIu64
            " video headers, %" PRIu64 " candidate samples\n",
            app.boundary_packets,
            app.video_headers,
            app.candidate_samples);
    }
    return app.exit_code;
}
