// Generates maven_truth.json with Maven's own ComparableVersion.
//
//   docker run --rm -v "$PWD":/t -w /t maven:3.9-eclipse-temurin-21 \
//     sh -c 'java -cp "$(ls /usr/share/maven/lib/maven-artifact-*.jar)" Gen.java'
import org.apache.maven.artifact.versioning.ComparableVersion;
import java.nio.file.*;
import java.util.*;

public class Gen {
    public static void main(String[] a) throws Exception {
        String[] v = {
            "1", "1.0", "1.0.0", "1.0.0.0", "1-0", "1.0-0", "1.1", "1.2", "1.10", "1.9", "2", "2.0", "10",
            "1-alpha", "1-alpha1", "1-alpha-1", "1-a1", "1-beta", "1-b2", "1-milestone", "1-m1", "1-rc", "1-cr",
            "1-rc1", "1-rc-1", "1-SNAPSHOT", "1.0-SNAPSHOT", "1-ga", "1-final", "1-release", "1-sp", "1-sp1",
            "1-foo", "1-foo2", "1.foo", "1-1", "1.1-1", "1-1.1", "1.0.1", "1.0.0.1", "1.0-alpha-1", "1.0-beta-2",
            "2.0.0-M1", "2.0.0.Final", "5.3.39", "6.1.14", "33.3.1-jre", "33.3.1-android", "32.1.3-jre",
            "1.7.36", "2.0.16", "3.0.0-beta1", "3.0.0-rc1", "1.0-RC1", "1.0-RC10", "1.0-RC2", "0.9", "1.0a",
            "1a", "1-a", "1.a", "1-x", "1.x", "1-1.x", "20240101", "1.0.0-20240101.123456-1", "1.2.3.4.5",
            "1-0.1", "1.0.0-", "1.", "1..0", "1-.1", "1.2-RELEASE", "1.2.RELEASE", "4.13.2", "4.13-beta-3",
            "1-xyz", "1-xyz1", "1-1-1", "1.0.0-x-y", "999999999", "9999999999", "99999999999999999999",
        };
        StringBuilder sb = new StringBuilder("{\"compare\":[\n");
        boolean first = true;
        for (String x : v) for (String y : v) {
            int c = Integer.signum(new ComparableVersion(x).compareTo(new ComparableVersion(y)));
            sb.append(first ? "" : ",\n").append("[\"").append(x).append("\",\"").append(y).append("\",").append(c).append("]");
            first = false;
        }
        sb.append("\n]}\n");
        Files.writeString(Path.of("maven_truth.json"), sb.toString());
        System.out.println(v.length * v.length + " comparisons with maven-artifact");
    }
}
