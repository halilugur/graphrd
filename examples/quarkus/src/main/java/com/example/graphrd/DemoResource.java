package com.example.graphrd;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.util.ArrayList;
import java.util.List;

import jakarta.annotation.security.PermitAll;
import jakarta.enterprise.context.ApplicationScoped;
import jakarta.inject.Inject;
import jakarta.ws.rs.GET;
import jakarta.ws.rs.Path;
import jakarta.ws.rs.Produces;
import jakarta.ws.rs.core.MediaType;

import org.eclipse.microprofile.config.inject.ConfigProperty;
import org.eclipse.microprofile.rest.client.inject.RestClient;

import io.agroal.api.AgroalDataSource;

/**
 * One GET that plays the whole scene: create the repository, load JSON-LD,
 * run a SPARQL query — and ask the same question as a SQL join over the
 * PostgreSQL wire protocol.
 */
@Path("/demo")
@ApplicationScoped
public class DemoResource {

    @Inject
    @RestClient
    GraphrdApi graphrd;

    @Inject
    AgroalDataSource ds;

    @ConfigProperty(name = "graphrd.repository")
    String repository;

    @GET
    @PermitAll
    @Produces(MediaType.TEXT_PLAIN)
    public List<String> run() throws IOException {
        List<String> out = new ArrayList<>();

        // 1. repository (409 = already exists; the REST client raises on it)
        int created;
        try {
            created = graphrd.createRepository("{\"id\":\"" + repository + "\"}").getStatus();
        } catch (jakarta.ws.rs.WebApplicationException e) {
            created = e.getResponse().getStatus();
        }
        out.add(created == 409 ? "repository already there" : "repository created");

        // 2. load the JSON-LD document from the class path
        try (InputStream in = getClass().getResourceAsStream("/data.json")) {
            String jsonLd = new String(in.readAllBytes(), StandardCharsets.UTF_8);
            graphrd.loadJsonLd(repository, jsonLd);
            out.add("loaded JSON-LD");
        }

        // 3. who does Alice know, and where do they work?
        String sparql = """
                PREFIX ex: <http://example.org/>
                SELECT ?friend ?company WHERE {
                  ex:alice ex:knows ?f .
                  ?f ex:name ?friend .
                  OPTIONAL { ?f ex:worksFor ?c . ?c ex:name ?company }
                }""";
        out.add("SPARQL answer: " + graphrd.select(repository, sparql));

        // 4. the same question as a SQL join
        out.add("SQL join over the same data:");
        String sql = "SELECT p.name AS person, c.name AS employer "
                + "FROM Person p JOIN Company c ON p.worksFor = c.id";
        try (Connection c = ds.getConnection();
             PreparedStatement ps = c.prepareStatement(sql);
             ResultSet rs = ps.executeQuery()) {
            while (rs.next()) {
                out.add("  " + rs.getString("person") + " works at " + rs.getString("employer"));
            }
        } catch (Exception e) {
            out.add("  (SQL skipped: " + e.getMessage() + ")");
        }
        return out;
    }
}
