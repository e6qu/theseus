// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.ProtectionDomain;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * The Theseus application-coverage agent for JVM workloads.
 *
 * <p>The agent records class-load coverage: every application class the JVM
 * defines is one coverage point, reported through the same first-hit serial
 * line protocol as the C, LLVM, and Go frontends. Classes are loaded lazily,
 * so a recorded class is a class the run actually touched.
 *
 * <p>The agent is built once and stays generic; the identity is supplied at
 * attach time as the agent options string
 * {@code process,module,build_sha256}, exactly as the {@code theseus
 * coverage java} frontend records it in the locked coverage manifest. Each
 * class's coverage point is the first eight bytes of the SHA-256 digest of
 * its internal name, emitted as {@code 0x} plus sixteen lowercase hex
 * digits - byte-for-byte the offsets the frontend lists in the build-scoped
 * symbol map.
 *
 * <p>Application classes are the ones whose defining class loader exists:
 * the JVM's own classes are defined by the bootstrap loader and carry no
 * loader. The agent never rewrites bytecode; the transformer only observes
 * definitions and returns {@code null}. The agent class implements the
 * transformer itself and defines no other classes, so the packaged JAR is
 * one self-contained class file.
 */
public class TheseusCoverageAgent implements ClassFileTransformer {

  private static final Set<String> SEEN = ConcurrentHashMap.newKeySet();

  private static volatile String prefix = null;

  /** Deterministic coverage-point identity for one class, shared with the frontend. */
  static String offsetFor(String internalName) {
    try {
      MessageDigest digest = MessageDigest.getInstance("SHA-256");
      byte[] hashed = digest.digest(internalName.getBytes(StandardCharsets.UTF_8));
      StringBuilder offset = new StringBuilder("0x");
      for (int index = 0; index < 8; index++) {
        offset.append(String.format("%02x", hashed[index]));
      }
      return offset.toString();
    } catch (Exception failure) {
      // SHA-256 is present in every supported JVM; this keeps the
      // transformer total so coverage never breaks the workload.
      return null;
    }
  }

  /** Entry point for {@code -javaagent:theseus-coverage-agent.jar=process,module,build_sha256}. */
  public static void premain(String options, Instrumentation instrumentation) {
    if (options == null || options.isBlank()) {
      throw new IllegalArgumentException(
          "theseus-coverage-agent requires process,module,build_sha256 options");
    }
    String[] identity = options.split(",");
    if (identity.length != 3
        || identity[0].isBlank()
        || identity[1].isBlank()
        || identity[2].isBlank()) {
      throw new IllegalArgumentException(
          "theseus-coverage-agent options must be process,module,build_sha256");
    }
    for (String field : identity) {
      if (field.length() > 64 || !field.matches("[A-Za-z0-9._-]+")) {
        throw new IllegalArgumentException("theseus-coverage-agent identity is invalid: " + field);
      }
    }
    prefix = "THES:COV:v1:" + identity[0] + ":" + identity[1] + ":" + identity[2] + ":";
    instrumentation.addTransformer(new TheseusCoverageAgent());
  }

  @Override
  public byte[] transform(
      ClassLoader loader,
      String className,
      Class<?> beingDefined,
      ProtectionDomain domain,
      byte[] bytes) {
    // Application classes have a defining loader; JVM classes do not.
    if (loader != null && className != null && prefix != null) {
      String offset = offsetFor(className);
      if (offset != null && SEEN.add(className)) {
        // One line on stderr; the pivot forwards instrumentation records to
        // the console and keeps them out of command output, exactly like
        // the C and Go runtimes.
        System.err.println(prefix + offset);
      }
    }
    return null;
  }
}
