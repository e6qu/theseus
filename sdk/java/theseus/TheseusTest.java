// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

package theseus;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;

/** A dependency-free, self-checking protocol test: the emitted lines must
 * be byte-identical to the Rust SDK's TtyChannel protocol, because the
 * host-side property layer matches them as needles. Run with {@code java
 * -cp build theseus.TheseusTest}; a nonzero exit is a failure. */
public final class TheseusTest {

  private static int failures = 0;

  private static void check(String what, Object got, Object want) {
    if (!got.equals(want)) {
      failures++;
      System.out.println("FAIL " + what + ": got " + got + ", want " + want);
    }
  }

  private static Theseus channel(String input, ByteArrayOutputStream out) {
    return new Theseus(out, new ByteArrayInputStream(input.getBytes(StandardCharsets.UTF_8)));
  }

  public static void main(String[] args) throws IOException {
    // Protocol lines match the Rust SDK byte-for-byte.
    var out = new ByteArrayOutputStream();
    var channel = channel("", out);
    channel.marker(0x42);
    channel.assertion("no_data_loss", true);
    channel.assertion("stale_read", false);
    channel.checkpoint("write");
    check(
        "protocol lines",
        out.toString(StandardCharsets.UTF_8),
        "THES:M:42\n"
            + "THES:ASSERT:no_data_loss:pass\n"
            + "THES:ASSERT:stale_read:fail\n"
            + "THES:CHECKPOINT:write\n");

    // The assignment parser accepts the locked form and rejects absences.
    check("assignment parse", Theseus.choiceAssignment("mode=1,retry=0", "mode"), 1);
    try {
      Theseus.choiceAssignment("mode=1", "absent");
      check("absent assignment", "accepted", "rejected");
    } catch (IOException expected) {
      check("absent assignment", "rejected", "rejected");
    }

    // When CI runs with THESEUS_CHOICES set, exercise the full choice path
    // including the recorded line.
    String assignments = System.getenv(Theseus.CHOICES_ENV);
    if (assignments != null && assignments.contains("mode=")) {
      var choiceOut = new ByteArrayOutputStream();
      var choiceChannel = channel("", choiceOut);
      int selected = choiceChannel.choice("mode", 2);
      check("choice value", selected, 1);
      check("choice line", choiceOut.toString(StandardCharsets.UTF_8), "THES:CHOICE:mode:2:1\n");
    }

    // Events and commands skip unrelated console traffic.
    var events = channel(
        "kernel: random: crng init done\nTHES:E:7\n", new ByteArrayOutputStream());
    check("event", events.nextEvent(), 7);

    var commands = channel(
        "THES:CHECKPOINT:write\nTHES:SHELL:operation:{\"name\":\"write\"}\n",
        new ByteArrayOutputStream());
    String[] received = commands.nextCommandAny(new String[] {
        "THES:SHELL:operation:", "THES:SERVICE:action:",
    });
    check("command index", received[0], "0");
    check("command", received[1], "{\"name\":\"write\"}");

    // Event rounds echo markers until the terminator, then MARKER_DONE.
    var round = new ByteArrayOutputStream();
    var roundChannel = channel("THES:E:01\nTHES:E:ff\nTHES:E:00\n", round);
    roundChannel.eventRound();
    check(
        "event round",
        round.toString(StandardCharsets.UTF_8),
        "THES:M:01\nTHES:M:ff\nTHES:M:ff\n");

    // An unstable choice name fails early, matching the Rust SDK.
    var unstable = channel("", new ByteArrayOutputStream());
    try {
      unstable.choice("bad name", 2);
      check("unstable name", "accepted", "rejected");
    } catch (IOException expected) {
      check("unstable name", "rejected", "rejected");
    }

    if (failures > 0) {
      System.out.println(failures + " check(s) failed");
      System.exit(1);
    }
    System.out.println("theseus java sdk protocol: ok");
  }
}
