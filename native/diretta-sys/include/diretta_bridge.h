#ifndef SPLAYER_DIRETTA_BRIDGE_H
#define SPLAYER_DIRETTA_BRIDGE_H

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SPLAYER_DIRETTA_TEXT_CAPACITY 256

typedef struct {
    char id[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char ipv6_addr[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char full_addr[SPLAYER_DIRETTA_TEXT_CAPACITY];
    int32_t if_idx;
    char target_name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char output_name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    char model_name[SPLAYER_DIRETTA_TEXT_CAPACITY];
    uint32_t mtu;
} SPlayerDirettaDevice;

typedef bool (*SPlayerDirettaNextBlock)(void* context, const uint8_t** data, size_t* len);
typedef void (*SPlayerDirettaReleaseBlock)(void* context);

const char* splayer_diretta_last_error(void);
size_t splayer_diretta_scan(SPlayerDirettaDevice* devices, size_t capacity);

void* splayer_diretta_open_direct(
    const char* target_id,
    uint32_t sample_rate,
    uint16_t channels,
    uint8_t storage_bits,
    void* source_context,
    SPlayerDirettaNextBlock next_block,
    SPlayerDirettaReleaseBlock release_block
);

void* splayer_diretta_open_dsd_direct(
    const char* target_id,
    uint32_t bit_rate,
    uint16_t channels,
    bool source_lsb_first,
    bool* wire_lsb_first,
    void* source_context,
    SPlayerDirettaNextBlock next_block,
    SPlayerDirettaReleaseBlock release_block
);

bool splayer_diretta_play(void* opaque);
bool splayer_diretta_pause(void* opaque);
void splayer_diretta_close(void* opaque);

#ifdef __cplusplus
}
#endif

#endif // SPLAYER_DIRETTA_BRIDGE_H
