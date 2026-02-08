MEMORY
{
  /* S140 SoftDevice v7.3.0 reserves:
   * - FLASH: 0x00000000 - 0x00026FFF (156K)
   * - RAM:   0x20000000 - 0x20007FFF (32K)
   *
   * Last 2 pages (8K) reserved for bonding storage:
   * - BONDING: 0x000FE000 - 0x000FFFFF
   */
  FLASH : ORIGIN = 0x00027000, LENGTH = 860K
  RAM   : ORIGIN = 0x20008000, LENGTH = 224K
}
