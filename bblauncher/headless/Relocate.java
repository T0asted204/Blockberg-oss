import org.objectweb.asm.*;
import org.objectweb.asm.commons.*;
import java.nio.file.*;
import java.util.stream.*;

/** Rewrites every reference to org.objectweb.asm.* in the class files under a directory to a private package. */
public class Relocate {
    static final String FROM = "org/objectweb/asm/", TO = "io/github/headlesshq/headlessmc/lwjgl/shadow/asm/";
    public static void main(String[] a) throws Exception {
        Path root = Paths.get(a[0]), out = Paths.get(a[1]);
        Remapper rm = new Remapper() { @Override public String map(String n) { return n.startsWith(FROM) ? TO + n.substring(FROM.length()) : n; } };
        try (Stream<Path> s = Files.walk(root)) {
            for (Path p : s.filter(x -> x.toString().endsWith(".class")).collect(Collectors.toList())) {
                ClassReader cr = new ClassReader(Files.readAllBytes(p));
                ClassWriter cw = new ClassWriter(0);
                cr.accept(new ClassRemapper(cw, rm), 0);
                String name = rm.map(cr.getClassName()) + ".class";
                Path dst = out.resolve(name);
                Files.createDirectories(dst.getParent());
                Files.write(dst, cw.toByteArray());
            }
        }
    }
}
