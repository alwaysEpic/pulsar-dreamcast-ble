MEMORY
{
  /* S140 SoftDevice v7.3.0 reserves:
   * - FLASH: 0x00000000 - 0x00026FFF (156K)
   * - RAM:   0x20000000 - 0x20007FFF (32K)
   *
   * App region ends at 0xF1000. Above it, in order:
   * - 0xF1000 panic log, 0xF2000 name/profile pref, 0xF3000 bond storage
   *   (the app-data window: below Adafruit's bootloader on dev boards,
   *   inside secure-DFU NRF_DFU_APP_DATA_AREA on retail — OTA-safe)
   * - 0xF4000 bootloader, 0xFE000 MBR params, 0xFF000 bootloader settings
   */
  FLASH : ORIGIN = 0x00027000, LENGTH = 808K
  RAM   : ORIGIN = 0x20008000, LENGTH = 224K
}
