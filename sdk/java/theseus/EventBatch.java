// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

package theseus;

import java.io.IOException;
import java.util.Map;
import java.util.TreeMap;

/**
 * Application events batch on the channel and flush as one compact JSON
 * line per event, in submission order, immediately before the next
 * checkpoint - one ordered timeline that the property layer's JSON
 * predicates and the temporal queries evaluate without translation.
 *
 * <p>Events carry a deterministic sequence number assigned at submission.
 * Wall-clock time is deliberately absent: it would break replay. String
 * values are JSON-escaped; numbers and booleans render literally; nested
 * {@code Map} and {@code Iterable} values render recursively.
 */
final class EventBatch {

  private final StringBuilder lines = new StringBuilder();
  private long seq;

  boolean isEmpty() {
    return lines.length() == 0;
  }

  /** Queue one event with the given fields; the SDK adds {@code seq}.
   * Keys render in sorted order so replay is byte-stable, matching the Go
   * module. */
  void add(Map<String, ?> fields) {
    seq++;
    Map<String, Object> ordered = new TreeMap<>();
    ordered.put("seq", seq);
    if (fields != null) {
      ordered.putAll(fields);
    }
    lines.append('{');
    boolean first = true;
    for (Map.Entry<String, Object> entry : ordered.entrySet()) {
      if (!first) {
        lines.append(',');
      }
      first = false;
      appendKey(lines, entry.getKey());
      lines.append(':');
      appendValue(lines, entry.getValue());
    }
    lines.append('}');
    lines.append('\n');
  }

  /** Write every queued line to the channel output and clear the batch. */
  void flushTo(Appendable out) throws IOException {
    out.append(lines);
    lines.setLength(0);
  }

  private static void appendValue(StringBuilder line, Object value) {
    if (value == null) {
      line.append("null");
    } else if (value instanceof CharSequence) {
      appendString(line, value.toString());
    } else if (value instanceof Map<?, ?> map) {
      line.append('{');
      boolean first = true;
      for (Map.Entry<?, ?> entry : map.entrySet()) {
        if (!first) {
          line.append(',');
        }
        first = false;
        appendKey(line, String.valueOf(entry.getKey()));
        line.append(':');
        appendValue(line, entry.getValue());
      }
      line.append('}');
    } else if (value instanceof Iterable<?> items) {
      line.append('[');
      boolean first = true;
      for (Object item : items) {
        if (!first) {
          line.append(',');
        }
        first = false;
        appendValue(line, item);
      }
      line.append(']');
    } else {
      line.append(value);
    }
  }

  private static void appendString(StringBuilder line, String value) {
    line.append('"');
    for (int index = 0; index < value.length(); index++) {
      char character = value.charAt(index);
      switch (character) {
        case '"' -> line.append("\\\"");
        case '\\' -> line.append("\\\\");
        case '\n' -> line.append("\\n");
        case '\r' -> line.append("\\r");
        case '\t' -> line.append("\\t");
        default -> {
          if (character < 0x20) {
            line.append(String.format("\\u%04x", (int) character));
          } else {
            line.append(character);
          }
        }
      }
    }
    line.append('"');
  }

  private static void appendKey(StringBuilder line, String key) {
    appendString(line, key);
  }
}
