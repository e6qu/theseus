// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

package theseus;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;

/**
 * The Theseus guest SDK for Java services.
 *
 * <p>It exposes the same vocabulary as the Rust SDK's TtyChannel and the Go
 * module over the same serial-line protocol: markers, named runtime
 * assertions, operation checkpoints, bounded structured choices consumed
 * from the {@code THESEUS_CHOICES} environment variable, host events, and
 * the command receiver the image service adapters use. Lines are written
 * byte-identically to the other SDKs, because the host-side property layer
 * matches them as needles.
 *
 * <p>Transports are injectable so the protocol is testable without a UART;
 * {@link #console()} opens {@code /dev/ttyS0}.
 */
public final class Theseus {

  /** Stable line prefixes and the choice environment variable. */
  public static final String MARKER_PREFIX = "THES:M:";
  /** Host-to-guest event line prefix. */
  public static final String EVENT_PREFIX = "THES:E:";
  /** Named assertion result prefix. */
  public static final String ASSERTION_PREFIX = "THES:ASSERT:";
  /** Operation checkpoint prefix. */
  public static final String CHECKPOINT_PREFIX = "THES:CHECKPOINT:";
  /** Structured choice record prefix. */
  public static final String CHOICE_PREFIX = "THES:CHOICE:";
  /** The environment variable carrying the locked choice assignments. */
  public static final String CHOICES_ENV = "THESEUS_CHOICES";

  /** Boot and round markers, shared with the control-channel protocol. */
  public static final int MARKER_BOOT = 0x42;
  /** The round-complete marker. */
  public static final int MARKER_DONE = 0xFF;
  /** The host's event-round terminator. */
  public static final int EVENT_TERMINATOR = 0x00;

  private final BufferedWriter out;
  private final BufferedReader in;

  /** Build a channel over an explicit transport, which keeps the protocol
   * testable without a UART. */
  public Theseus(OutputStream out, InputStream in) {
    this.out = new BufferedWriter(new OutputStreamWriter(out, StandardCharsets.UTF_8));
    this.in = new BufferedReader(new InputStreamReader(in, StandardCharsets.UTF_8));
  }

  /** Open the console UART for the channel, the default guest transport. */
  public static Theseus console() throws IOException {
    OutputStream out =
        new java.io.FileOutputStream("/dev/ttyS0", false);
    InputStream in = new java.io.FileInputStream("/dev/ttyS0");
    return new Theseus(out, in);
  }

  /** Emit a marker byte; the host sees a {@code THES:M:xx} line. */
  public void marker(int b) throws IOException {
    out.write(String.format("%s%02x%n", MARKER_PREFIX, b));
    out.flush();
  }

  /** Report a named runtime assertion. The name must be stable across
   * runs; campaign properties match the line directly as an auditable
   * property witness rather than inferring state from host timing. */
  public void assertion(String name, boolean passed) throws IOException {
    out.write(ASSERTION_PREFIX + name + ":" + (passed ? "pass" : "fail"));
    newLine();
  }

  /** Mark the end of one workload operation, giving applications a stable
   * serial checkpoint protocol without requiring the host to infer
   * progress. */
  public void checkpoint(String name) throws IOException {
    out.write(CHECKPOINT_PREFIX + name);
    newLine();
  }

  /** Consume one named structured choice and record it immediately before
   * the workload uses the value. Theseus injects the exact assignment
   * through {@code THESEUS_CHOICES}; the name and bound are checked at the
   * call site so a replay fails early if the decision contract changes. */
  public int choice(String name, int upperExclusive) throws IOException {
    if (upperExclusive <= 0
        || upperExclusive > 256
        || name.isEmpty()
        || name.length() > 64) {
      throw new IOException("choice needs a stable name and a bound from 1 through 256");
    }
    for (int index = 0; index < name.length(); index++) {
      char c = name.charAt(index);
      boolean allowed =
          (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '_';
      if (!allowed) {
        throw new IOException("choice needs a stable name and a bound from 1 through 256");
      }
    }
    int selected = choiceAssignment(System.getenv(CHOICES_ENV), name);
    if (selected >= upperExclusive) {
      throw new IOException("structured choice is outside the requested bound");
    }
    out.write(CHOICE_PREFIX + name + ":" + upperExclusive + ":" + selected);
    newLine();
    return selected;
  }

  /** Parse one comma-separated name=value assignment out of the choice
   * environment. */
  static int choiceAssignment(String encoded, String name) throws IOException {
    if (encoded == null) {
      throw new IOException("THESEUS_CHOICES does not contain this campaign assignment");
    }
    for (String assignment : encoded.split(",")) {
      int split = assignment.indexOf('=');
      if (split > 0 && assignment.substring(0, split).equals(name)) {
        try {
          return Integer.parseInt(assignment.substring(split + 1));
        } catch (NumberFormatException failure) {
          throw new IOException("structured choice " + name + " is not a number");
        }
      }
    }
    throw new IOException("THESEUS_CHOICES does not contain this campaign assignment");
  }

  /** Read the next event byte, blocking and skipping non-channel lines
   * such as kernel logs. */
  public int nextEvent() throws IOException {
    while (true) {
      String line = readLine();
      if (line.startsWith(EVENT_PREFIX)) {
        try {
          return Integer.parseInt(line.substring(EVENT_PREFIX.length()), 16);
        } catch (NumberFormatException ignored) {
          // Not a channel line after all; keep skipping.
        }
      }
    }
  }

  /** Read the next host command with a stable line prefix. Image-backed
   * service adapters use this after their boot barrier to receive a
   * declared campaign operation without application instrumentation. */
  public String nextCommand(String prefix) throws IOException {
    String[] received = nextCommandAny(new String[] {prefix});
    return received[1];
  }

  /** Read the next host command matching one of several stable line
   * prefixes. Returns {@code [prefixIndex, command]}: the index lets an
   * adapter support more than one declared protocol without interpreting
   * application logs. */
  public String[] nextCommandAny(String[] prefixes) throws IOException {
    while (true) {
      String line = readLine();
      for (int index = 0; index < prefixes.length; index++) {
        if (line.startsWith(prefixes[index])) {
          return new String[] {Integer.toString(index), line.substring(prefixes[index].length())};
        }
      }
    }
  }

  /** The standard event round: echo events as markers until the
   * terminator, then emit {@code MARKER_DONE}. */
  public void eventRound() throws IOException {
    while (true) {
      int event = nextEvent();
      if (event == EVENT_TERMINATOR) {
        break;
      }
      marker(event);
    }
    marker(MARKER_DONE);
  }

  /** One console line without its terminator; a closed console is an error
   * the caller cannot recover from. */
  private String readLine() throws IOException {
    String line = in.readLine();
    if (line == null) {
      throw new IOException("console closed");
    }
    return line;
  }

  private void newLine() throws IOException {
    out.write("\n");
    out.flush();
  }
}
