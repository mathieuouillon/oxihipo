// Cross-implementation read benchmark (Java / jnp-hipo4).
// Identical workload to xbench_rs.rs and xbench_cpp.cc:
//   scan every event, read REC::Particle pid(int) + px(float), accumulate.
// Uses pre-resolved column indices, matching the other two.
import org.jlab.jnp.hipo4.data.*;
import org.jlab.jnp.hipo4.io.*;

public class XBenchJava {
    public static void main(String[] args) {
        String path = args[0];
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        double best = Double.MAX_VALUE, first = -1;
        long events = 0, rows = 0, csp = 0; double csx = 0;
        for (int it = 0; it < iters; it++) {
            HipoReader r = new HipoReader();
            r.setDebugMode(0);
            r.open(path);
            Schema sch = r.getSchemaFactory().getSchema("REC::Particle");
            Bank b = new Bank(sch);
            Event ev = new Event();
            // Column order is the schema's entry order — the public equivalent
            // of the pre-resolved handles/indices the other two use.
            int iPid = sch.getEntryList().indexOf("pid");
            int iPx = sch.getEntryList().indexOf("px");

            long t0 = System.nanoTime();
            long sp = 0; double sx = 0; long e = 0, rw = 0;
            while (r.hasNext()) {
                r.nextEvent(ev);
                ev.read(b);
                e++;
                int n = b.getRows();
                for (int i = 0; i < n; i++) sp += b.getInt(iPid, i);
                for (int i = 0; i < n; i++) sx += b.getFloat(iPx, i);
                rw += n;
            }
            double dt = (System.nanoTime() - t0) / 1e9;
            if (sp == Long.MIN_VALUE && sx == 1.2345) System.out.print(""); // keep
            events = e; rows = rw; csp = sp; csx = sx;
            if (first < 0) first = dt;
            if (dt < best) best = dt;
            r.close();
        }
        System.out.printf("java\t%.4f\t%.4f\t%d\t%d\t%d\t%.3f%n", first, best, events, rows, csp, csx);
    }
}
