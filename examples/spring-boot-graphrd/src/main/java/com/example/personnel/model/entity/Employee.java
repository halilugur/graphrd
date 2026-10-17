package com.example.personnel.model.entity;

import org.graphrd.rdf.RdfId;
import org.graphrd.rdf.RdfProperty;
import org.graphrd.rdf.RdfType;

import lombok.AllArgsConstructor;
import lombok.Getter;
import lombok.NoArgsConstructor;
import lombok.Setter;

/** An employee; {@code worksFor} is an IRI reference to a {@link Company}, read eagerly. */
@Getter
@Setter
@NoArgsConstructor
@AllArgsConstructor
@RdfType("Employee")
public class Employee {

    @RdfId(generated = true)
    String id;

    @RdfProperty("name")
    String name;

    @RdfProperty("email")
    String email;

    @RdfProperty("age")
    Integer age;

    @RdfProperty("salary")
    Integer salary;

    @RdfProperty("hired")
    String hired;

    @RdfProperty("worksFor")
    Company worksFor;
}
