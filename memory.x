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

/* No placement guard here any more.
 *
 * An earlier revision pinned the Maple bus code into a `.maple_text` section ahead of
 * .text, with 0x18 of padding, so that the RX sampling loop landed at 0x1c
 * mod 32: the 14-byte loop straddles a 16-byte flash line, its cycle count is
 * the sample rate the decode thresholds are calibrated to, and every build
 * with it 16-byte aligned failed Control (v210, v253, v257, v261, v278-v280).
 * v282 measured it: 11.00 cycles per sample at the two 16-byte-aligned phases
 * against 8.00 at the other six.
 *
 * Route (a) removed that loop from the boards that ship. `read_packet_bulk`
 * selects its capture stage by `#[cfg]`, and on the carrier and the XIAO
 * (which imply `spim-capture`) it is `capture_on_spim` — clocked by hardware,
 * with no fallback: absent capture hardware fails the poll. `wait_and_sample`,
 * which holds the sampling loop, is not compiled into those builds at all, so
 * there is no loop in a production binary to place.
 *
 * SCOPE — this is an argument about the RX sampler and nothing else:
 *
 * - Command TX still bit-bangs GPIO edges from the CPU. `delay_half_bit`
 *   spins to an absolute DWT deadline rather than counting a delay loop, so it
 *   does not accumulate placement-induced drift and it measures its own
 *   lateness — but the CPU still writes the edges, and instruction latency,
 *   interrupts and deadline overruns can still delay them. That is grounds for
 *   testing this build unpinned; it is NOT a claim that TX timing is
 *   independent of placement.
 * - The DK still compiles the sampler, and removing this section removed its
 *   placement control too. `check_timing_invariants.sh` answers that
 *   separately: `cpu` mode now rejects a 16-byte-aligned loop on any board
 *   that has one, which is the measured cause rather than one proven address.
 *
 * NOT YET RETIRED BY PROOF. This build is the candidate: the same tree with
 * the pad and the hold removed, so its placement differs by construction. The
 * pin is retired when a Control run on a pulsarv1 unit passes on it, not by this
 * comment. See ADR-018. */
