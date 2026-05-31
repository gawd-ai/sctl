#ifndef SCTL_COMMS_ABI_H
#define SCTL_COMMS_ABI_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define SCTL_COMMS_ABI_VERSION 1u
#define SCTL_COMMS_MAX_STR 128u
#define SCTL_COMMS_MAX_BANDS 128u

#define SCTL_COMMS_OK 0
#define SCTL_COMMS_ERR -1
#define SCTL_COMMS_ERR_UNSUPPORTED -2
#define SCTL_COMMS_ERR_INVALID -3
#define SCTL_COMMS_ERR_BUFFER_TOO_SMALL -4
#define SCTL_COMMS_ERR_MODEM_UNAVAILABLE -5
#define SCTL_COMMS_ERR_AT_FAILED -6
#define SCTL_COMMS_ERR_TUNNEL_CONNECTED -7
#define SCTL_COMMS_ERR_SCAN_RUNNING -8

/* Log levels passed to the host `log` callback. */
#define SCTL_COMMS_LOG_ERROR 1
#define SCTL_COMMS_LOG_WARN 2
#define SCTL_COMMS_LOG_INFO 3
#define SCTL_COMMS_LOG_DEBUG 4

/* `kind` argument to the host `speed_test` callback. */
#define SCTL_COMMS_SPEED_DOWNLOAD 1
#define SCTL_COMMS_SPEED_UPLOAD 2

#define SCTL_COMMS_CAP_LOCATION_GNSS (1ull << 0)
#define SCTL_COMMS_CAP_LINK_CELLULAR (1ull << 1)
#define SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL (1ull << 2)
#define SCTL_COMMS_CAP_CELLULAR_SCAN (1ull << 3)
#define SCTL_COMMS_CAP_RECOVERY_USB_CYCLE (1ull << 4)
#define SCTL_COMMS_CAP_RECOVERY_TUNNEL_WATCHDOG (1ull << 5)

typedef struct {
  const uint8_t *ptr;
  uintptr_t len;
} SctlCommsSlice;

typedef struct {
  uint8_t *ptr;
  uintptr_t cap;
  uintptr_t *len;
} SctlCommsMutSlice;

typedef struct {
  uint32_t abi_version;
  void *ctx;
  void (*log)(void *ctx, int32_t level, SctlCommsSlice msg);
  uint64_t (*now_ms)(void *ctx);
  int32_t (*at_command)(void *ctx, SctlCommsSlice command, uint64_t timeout_ms, SctlCommsMutSlice out);
  bool (*interface_has_ipv4)(void *ctx, SctlCommsSlice iface);
  int32_t (*speed_test)(void *ctx, int32_t kind, SctlCommsSlice url, SctlCommsSlice iface, uint64_t *out_bps);
  int32_t (*usb_cycle)(void *ctx, SctlCommsMutSlice out_detected_path);
} SctlCommsHostV1;

typedef struct {
  uint64_t capabilities;
  char detected_path[SCTL_COMMS_MAX_STR];
} SctlCommsProbeResult;

typedef struct {
  SctlCommsSlice provider;
  SctlCommsSlice device;
  SctlCommsSlice data_dir;
  bool gps_enabled;
  bool gps_auto_enable;
  uintptr_t gps_history_size;
  bool lte_enabled;
  bool lte_watchdog;
  SctlCommsSlice lte_interface;
  SctlCommsSlice speed_test_url;
  SctlCommsSlice speed_test_upload_url;
  SctlCommsSlice tunnel_url;
} SctlCommsOpenConfig;

typedef struct {
  bool tunnel_connected;
} SctlCommsStatusParams;

typedef struct {
  bool refresh;
  bool tunnel_connected;
} SctlCommsLinkPollParams;

#define SCTL_COMMS_BAND_MODE_AUTO 1
#define SCTL_COMMS_BAND_MODE_LOCKED 2

typedef struct {
  int32_t mode;
  uint16_t bands[SCTL_COMMS_MAX_BANDS];
  uintptr_t bands_len;
  uint16_t priority_band;
  bool has_priority_band;
  bool force;
  bool tunnel_connected;
} SctlCommsSetBandsParams;

typedef struct {
  uint16_t bands[SCTL_COMMS_MAX_BANDS];
  uintptr_t bands_len;
  bool include_speed_test;
  bool force;
  bool tunnel_connected;
} SctlCommsScanParams;

typedef struct {
  uint32_t abi_version;
  void *plugin_ctx;
  void (*destroy)(void *plugin_ctx);
  int32_t (*probe)(void *plugin_ctx, SctlCommsProbeResult *out);
  int32_t (*open)(void *plugin_ctx, const SctlCommsOpenConfig *config, SctlCommsMutSlice out_json);
  int32_t (*close)(void *plugin_ctx);
  int32_t (*status)(void *plugin_ctx, const SctlCommsStatusParams *params, SctlCommsMutSlice out_json);
  int32_t (*poll_location)(void *plugin_ctx, SctlCommsMutSlice out_json);
  int32_t (*disable_location)(void *plugin_ctx, SctlCommsMutSlice out_json);
  int32_t (*poll_link)(void *plugin_ctx, const SctlCommsLinkPollParams *params, SctlCommsMutSlice out_json);
  int32_t (*set_bands)(void *plugin_ctx, const SctlCommsSetBandsParams *params, SctlCommsMutSlice out_json);
  int32_t (*scan)(void *plugin_ctx, const SctlCommsScanParams *params, SctlCommsMutSlice out_json);
  int32_t (*usb_cycle)(void *plugin_ctx, SctlCommsMutSlice out_json);
} SctlCommsPluginV1;

typedef int32_t (*SctlCommsPluginInitV1)(const SctlCommsHostV1 *host, SctlCommsPluginV1 *out);

#endif
