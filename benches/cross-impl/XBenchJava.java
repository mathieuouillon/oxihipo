// Cross-implementation read benchmark — Java (jnp-hipo4).
//   XBenchJava <file.hipo> <scenario> [iters]
import org.jlab.jnp.hipo4.data.*;
import org.jlab.jnp.hipo4.io.*;

public class XBenchJava {
    public static void main(String[] args) {
        String path = args[0], scen = args[1];
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 10;

        double best = Double.MAX_VALUE, first = -1;
        long events = 0; double csum = 0;
        for (int it = 0; it < iters; it++) {
            HipoReader r = new HipoReader();
            r.setDebugMode(0);
            r.open(path);
            SchemaFactory sf = r.getSchemaFactory();
            Schema ps = sf.getSchema("REC::Particle");
            Schema es = sf.getSchema("REC::Event");
            Bank P = new Bank(ps), E = new Bank(es);
            Event ev = new Event();
            int iPid = ps.getEntryList().indexOf("pid");
            int iPx = ps.getEntryList().indexOf("px");
            int iPy = ps.getEntryList().indexOf("py");
            int iPz = ps.getEntryList().indexOf("pz");
            int iVz = ps.getEntryList().indexOf("vz");
            int iCh = ps.getEntryList().indexOf("charge");
            int iSt = ps.getEntryList().indexOf("status");
            int iC2 = ps.getEntryList().indexOf("chi2pid");
            int iEv = es.getEntryList().indexOf("evno");

            long t0 = System.nanoTime();
            double sum = 0; long e = 0;
            while (r.hasNext()) {
                r.nextEvent(ev);
                e++;
                if (scen.equals("count")) continue;
                if (scen.equals("bank1")) {
                    ev.read(E);
                    int n = E.getRows();
                    for (int i = 0; i < n; i++) sum += (double) E.getLong(iEv, i);
                    continue;
                }
                ev.read(P);
                int n = P.getRows();
                if (scen.equals("col1")) {
                    for (int i = 0; i < n; i++) sum += P.getInt(iPid, i);
                } else if (scen.equals("scan2")) {
                    for (int i = 0; i < n; i++) sum += P.getInt(iPid, i);
                    for (int i = 0; i < n; i++) sum += P.getFloat(iPx, i);
                } else if (scen.equals("wide")) {
                    for (int i = 0; i < n; i++) sum += P.getInt(iPid, i);
                    for (int i = 0; i < n; i++) sum += P.getFloat(iPx, i);
                    for (int i = 0; i < n; i++) sum += P.getFloat(iPy, i);
                    for (int i = 0; i < n; i++) sum += P.getFloat(iPz, i);
                    for (int i = 0; i < n; i++) sum += P.getFloat(iVz, i);
                    for (int i = 0; i < n; i++) sum += P.getByte(iCh, i);
                    for (int i = 0; i < n; i++) sum += P.getShort(iSt, i);
                    for (int i = 0; i < n; i++) sum += P.getFloat(iC2, i);
                } else { System.err.println("unknown scenario"); System.exit(2); }
            }
            double dt = (System.nanoTime() - t0) / 1e9;
            events = e; csum = sum;
            if (first < 0) first = dt;
            if (dt < best) best = dt;
            r.close();
        }
        System.out.printf("java\t%s\t%.5f\t%.5f\t%d\t%.3f%n", scen, first, best, events, csum);
    }
}
